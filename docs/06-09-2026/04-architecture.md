# Architecture de lucivy — état au 7 septembre 2026 au matin (4.0.1 publiée)

Rappel écrit pour être lu seul. Il complète
[`../05-09-2026/11-architecture.md`](../05-09-2026/11-architecture.md) (le
repli différé tel que conçu, la vitrine, le banc, la chaîne de présentation)
et [`../05-09-2026/07-architecture.md`](../05-09-2026/07-architecture.md)
(les formats 4.0, la requête en positions, les dérivés, la fusion), qui
restent exacts. Ici : ce qui a changé dans la journée du 6 et ce que la
publication a figé. La version anglaise publique est `ARCHITECTURE.md` à la
racine, alignée.

---

## 1. Ce que 4.0.1 est

Cinq crates au même numéro, quatre bindings, sept paquets npm, la page. Le
**dictionnaire partagé par shard est le défaut** (`SchemaConfig::
effective_sfx_version()` → 4 sauf `shared_dictionary: false` ou
`sfx_version` explicite ; `IndexSettings::default()` du crate bas niveau
reste 3 ; un index existant garde la version de son `meta.json`). Contrat
de format : 4.0 ouvre 3.0.x, 3.0.x n'ouvre pas 4.0, le premier commit
convertit — `test_compat_308` (fixture du wheel 3.0.8) et un index de 10 000
fichiers bâti par `main` (3.0.8) rouvert par v4, 10/10.

## 2. L'indexation en mode dictionnaire, en natif

```
segment (fil de construction) : tokens → lookup_or_mint par jeton distinct
   ├─ filtre de Bloom sur la clé d'internement (texte + forme) : « jamais minté » → mint direct
   ├─ sinon marche FST sur chaque partie vivante (lecteurs .termtexts ouverts une fois par champ)
   └─ sinon tranche (16) des textes en attente → id, ou mint (next_id++)
   écrit .newtexts (TTX3 avec ids) ET .newsfx (la FST de ces textes)
commit : meta.pending_segments += segments neufs ; dictionnaire vivant rouvert (générations + paires) ;
   forget_pending ; ids lus par TermTextsReaderV3::ids() (sans décoder les textes) ; tâche de fond si aucune
tâche (run_fold, un permis de fusion) : compact_parts(paires) → dict-<g> (passes FST et textes en parallèle),
   compaction au-delà de 8 générations, permutation du dictionnaire vivant, SuDictionaryFoldedMsg → l'acteur
   réécrit meta.json, supprime les paires plus nommées, GC ; boucle tant qu'il reste des paires
lecteur : refresh garde le dictionnaire tenu s'il est en avance du disque (next_generation) — la course du 6
recherche : LucivyHandle::search / ShardedHandle::search → wait_dictionary_fold (dictionary_wait, défaut) → reload
fermeture : wait_merging_threads → wait_settled (repli fini ET meta.json écrit)
```

Bornes : une tâche par index ; `LUCIVY_DICT_MAX_PENDING` (16) → repli
synchrone ; `LUCIVY_DICT_SYNC_FOLD=1`. Snapshots LUCE et deltas transportent
les paires nommées (bundle `<uuid>.<champ>.new`) ; le snapshot tolère une
paire absente pour un champ (`pair_files` nomme tous les champs, un segment
n'écrit que ceux où il a minté).

**Sur wasm32** : le chemin d'avant le 6 — pas de `.newsfx` par segment
(`sfx_dag_v3.rs`), repli synchrone au commit. Mesuré : le fond n'y gagne rien
et les FST par segment en parallèle montaient le pic de 256 Mo.

**Ce que ça coûte et rend** : 30 000 fichiers 23,0 s (v3 15,3, veille 32,2) ;
noyau 106,8 s (131), `derived_in_ram` 110,9 (134). Le chemin par jeton reste
35 s cumulées sur les fils (28 de FST, 7 M frappés à 2,9 µs) mais ne borne
rien en natif ; l'écart restant (~7,5 s) : 2,6 s d'attente finale (dernier
repli + compaction, gardée : un lecteur ne doit pas hériter de 9 générations)
et ~5 s non attribuées, à chronométrer **côté mur**.

## 3. Jaro-Winkler

`briques/jaro_winkler.rs::jaro_spans(needle, hay, slack, min_sim)` : une
occurrence par groupe de sous-chaînes chevauchantes à ±slack caractères de la
longueur de la requête, similarité ≥ seuil **et** ≤ slack éditions
(`within_edits`, Levenshtein sur chars) ; la plus similaire, ex æquo la plus
courte puis la plus à gauche. Même définition dans le harnais
(`grep_spans_jaro`). Le composite garde la meilleure similarité du document
comme palier de score. La borne en éditions est ce qui rend le résultat
indépendant du découpage en fenêtres.

## 4. L'ordre des résultats

`save_metas` trie les segments par (−max_doc, id) : deux segments de même
taille ne dépendent plus de l'ordre du gestionnaire (hash), donc une
réécriture de `meta.json` après fusion ou repli ne réordonne plus les
ex æquo ni les adresses de documents d'un lecteur rechargé.

## 5. La vitrine

```
registre lucivy_corpora (localStorage, la vue) ⇄ OPFS (la vérité, resynchronisé au démarrage)
   clés : lucivy · <corpus> · gh:owner/repo · user (/user_index) · snapshot (/user_snapshot)
un seul index en mémoire : closeAllOpen() avant toute indexation / ouverture, quelle que soit la porte
onglets = renderTabs() sur le registre : l'ouvert marqué, témoin sur celui qui s'ouvre (activateSlot),
   croix = dropSlot, ↻ sur la source lucivy seulement ; switchTab('demo'|'user') est un shim
budget OPFS = min(8 Gio, quota/2) ; ensureRoom(texte × 9) évince le moins récemment ouvert
   (jamais lucivy source ni user/snapshot) ; storageFullError traduit un quota atteint
terminal : index <corpus> | owner/repo[@branche] | URL github.com ; options à un ou deux tirets, valeur entre crochets
dictionnaire par défaut (?nodict) ; ?ram ; ?commitmb (8) ; ?commit ; ?merges ; ?verbose
```

## 6. La publication

`release.yml` : douze builds + **`checks`** (clippy `-D warnings`, lib avec et
sans features par défaut, `lucivy-core`, `lucivy-cpp`) en parallèle ; les
publications PyPI, npm (six paquets), `lucivy-wasm`, crates.io dépendent de
`checks`. `PUBLISH_ENABLED` (variable de dépôt) reste l'interrupteur ;
l'environnement `release` n'a pas de réviseur et n'accepte que les tags
`v*`. `pages.yml` déploie la page depuis `main` et bâtit les douze corpus.
Règle : tag seulement après un run CI vert sur le commit exact.

## 7. Ce qui n'a pas bougé

Formats 4.0 (`07` §2), requête en positions, dérivés et `derived_in_ram`,
fusion, sharding, filtre, fédération, LUCE/LUCID/LUCIDS, stockage blob
(`../28-08-2026/08-architecture.md`, `07`).
