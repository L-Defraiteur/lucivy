# Architecture de lucivy — état au 6 septembre 2026, matin (4.0.0, branche `v4`)

Rappel écrit pour être lu seul. Il complète [07](07-architecture.md) (les
formats 4.0, la requête en positions, les dérivés et l'option, la fusion),
qui reste exact : ici ce qui a bougé depuis — le playground comme vitrine,
le banc comparatif, la chaîne de présentation — et **le chemin d'indexation
en mode dictionnaire avec le repli différé** du 6 septembre. La version anglaise
publique est `ARCHITECTURE.md` à la racine (la page que Google montre en
premier), alignée ce soir.

---

## 1. Les crates, le numéro, le contrat

Cinq crates au même numéro, **4.0.0, non publié** (3.0.8 dernière sur PyPI,
npm, crates.io). Contrat vérifié par `test_compat_308` : 4.0 ouvre un index
3.0.x et rend ce que 3.0.x rendait ; 3.0.x n'ouvre pas 4.0 ; le premier
commit convertit. Les formats : [07](07-architecture.md) §2.

## 2. L'indexation en mode dictionnaire (`sfx_version` 4) — le chemin chaud, et le repli différé (6 septembre)

```
document ─ tokenizer ─ SfxCollectorV3::with_dictionary(slot, field)
   pour chaque jeton distinct du segment :
     dict.lookup_or_mint(field, key, text, meta)
        ├─ field.lookup(text, meta) : pour chaque partie vivante (générations, puis paires en attente)
        │     FST get(partition + minuscules(own)) → parents où l'overlap concorde
        │     → forme égale → texte confirmé dans .termtexts (lecteur ouvert une fois par champ)
        └─ sinon : tranche (16, par hachage) des textes en attente → id, ou mint (next_id++)
   ordinaux locaux → ids globaux (.gmap)
   textes mintés → .newtexts (TTX3 avec ids) ET .newsfx (leur FST, bâtie sur le fil du segment)
au commit (par shard) : meta.pending_segments += les segments neufs ; dictionnaire vivant rouvert
   (générations + paires) ; forget_pending ; tâche de fond si aucune ne tourne
tâche de fond (run_fold, un permis de fusion) : boucle tant qu'il reste des paires —
   compact_parts(paires) → dict-<g> (passes FST et textes en parallèle en natif),
   compaction au-delà de 8 générations, permutation du dictionnaire vivant (RAM),
   puis SuDictionaryFoldedMsg → l'acteur réécrit meta.json, supprime les paires plus nommées, GC
recherche : LucivyHandle::search / ShardedHandle::search → wait_dictionary_fold (défaut) → reload si permuté
fermeture : wait_merging_threads → wait_settled (repli fini et meta.json écrit)
```

**Pourquoi.** Mesuré le 6 au petit matin (`10` §9) : le chemin par jeton est
lourd (14 M appels sur 30 000 fichiers) mais tourne sur les fils des
collecteurs, en parallèle du flux ; ce qui faisait le mur était le
**commit**, où l'écriture de la génération (8,8 s cumulées), la compaction
(3,4) et la réouverture (1,4) s'enchaînaient pendant que rien d'autre
n'avançait. Bâtir une FST en ordre de clés est séquentiel par nature
(1,2 µs la clé, fusion comme construction) : la seule issue était de ne
plus rien bâtir au commit.

**Invariants.** Les ids sont stables et ajoutés seulement : une paire et la
génération qui l'absorbe répondent pareil, un repli ne touche aucun
segment. Un texte minté reste dans les tranches en attente jusqu'à ce que
le dictionnaire vivant le porte (paire ou génération) : jamais deux ids pour
un texte. Ce que `meta.json` nomme existe sur disque : une paire n'est
supprimée que par le commit ou le message de repli **après** l'écriture
d'un `meta.json` qui ne la nomme plus ; un `dict-<g>` en cours d'écriture
est plus neuf que tout ce qui est vivant, le GC le garde ; un processus
mort entre le repli et l'écriture rouvre sur les paires, le numéro `g` est
réutilisé après `remove_leftovers`. Une seule tâche de repli par index ;
au-delà de `LUCIVY_DICT_MAX_PENDING` (16) paires, le commit attend la tâche
et replie lui-même ; `LUCIVY_DICT_SYNC_FOLD=1` rend le commit d'avant.
**Sur wasm32, tout le chemin d'avant est gardé** : pas de `.newsfx` bâti par
le segment (`sfx_dag_v3.rs`, `cfg!(target_arch = "wasm32")`), repli
synchrone au commit qui bâtit les FST manquantes une à la fois. Mesuré dans
Chrome : le repli de fond n'y gagne rien (2.6.0 41 → 42 s, Godot 30 → 36 s :
peu de fils) et **les FST par segment bâties en parallèle montaient le pic
mémoire** de 2 023 à 2 279 Mo sur la 2.6.0 et de 1 778 à 1 894 sur Godot ;
le repli synchrone seul ne le rendait pas (2 279), les deux ensemble oui
(2 023 en 42 s ; Godot 1 766 en 31 s).

**Ce que voit une requête.** Par défaut jamais les paires : la recherche
attend le repli en cours (une seconde au plus en natif sur le noyau) puis
recharge ; `dictionary_wait: false` (config du schéma, `LUCIVY_DICT_WAIT=0`)
cherche tout de suite sur plus de parties. Les snapshots LUCE et les deltas
transportent les paires nommées (bundle `<uuid>.<champ>.new`, préfixe de ses
deux fichiers) ; l'export attend d'abord l'état posé (`wait_merges_quiet`).

**Refusé, mesuré.** Le cache des clés *trouvées* dans une génération (5,7 M
de marches FST évitées, autant de verrou en plus, 32 Mo par shard) ; moins
de générations vivantes (4 : 36 s, 2 : 55 s — la compaction coûte plus que
les `get` économisés).

## 3. Le playground comme vitrine

```
index.html (module)                    corpora.json ─ tools/build_corpus.py ─ corpus-<nom>.tar.gz
  ├─ démo : lucivy-source.tar.gz          (source github:owner/repo@ref ou URL, licence, panel,
  ├─ terminal : $ lucivy <verbe|valeur>     stats écrits par le script ; pages.yml les bâtit)
  │     search "…" | index <nom> | open | drop | list | help
  ├─ indexFiles : commit tous les 2 000 fichiers OU 8 Mo de texte (?commitmb)
  ├─ extractTarGz : ustar prefix, GNU L, PAX x (noms longs)
  └─ drop : lucivy.dropIndex(path) (worker, WASMFS) puis removeEntry (main thread)
```

- **Un index ouvert vit en RAM, les autres en OPFS** ; le registre
  (`localStorage` `lucivy_corpora`) est la vue amicale, les répertoires OPFS
  la vérité.
- **Le pic mémoire suit la taille des segments** : commiter par volume a
  ramené Godot de 3,3 à 1,8 Go. Les seuils à connaître : plancher ~1,5 Go
  de l'indexation, 4 Go d'adresses, refus au-delà de ~220 Mo de texte.
- `?ram` (`derived_in_ram`) reste une option : −26 % d'OPFS mais +524 Mo au
  pic d'indexation du noyau.
- La page : sous-titre (exactitude vérifiée, transaction), deux rangées de
  cartes, « Numbers » (panel du noyau 4.0), **« One corpus, one truth »**
  (le tableau des trébuchements, deux lignes constatées, ce qu'ils font
  mieux, la commande), « Browser against native » (2.6.0), limites honnêtes.
  Déployée par `pages.yml` depuis `main` seulement.

## 4. Le banc comparatif

```
benches/compare_engines.sh <corpus> [travail]
  ├─ harnais lucivy (v3_ground_truth_demo) × 3 layouts → lucivy-*.log, .bytes
  ├─ harnais lucivy, V3_QUERIES « où les questions diffèrent », span cap 0 → lucivy-stumble.log
  ├─ compare_tantivy.rs (CMP_CORPUS, CMP_OUT) → tantivy.json  [défaut, NgramTokenizer]
  ├─ compare_elasticsearch.py (ES_URL) → elasticsearch.json   [standard, trigrammes + wildcard]
  └─ compare_engines_report.py → compare_engines.md (4 parties ; gras = vérité)
```

Principes : chaque moteur configuré au mieux, jamais un homme de paille ;
chaque ligne porte la clé `truth` en syntaxe `V3_QUERIES` ; ce qu'ils font
mieux reste écrit ; une question qu'un moteur ne peut pas poser est une
ligne, pas une erreur de comptage. tantivy : la `PhraseQuery` de trigrammes
rend 0 (positions à 0), donc `verified_substring` = ET de trigrammes
(`BooleanQuery::intersection`) puis lecture du texte stocké de chaque
candidat ; un trigramme unique est un `TermQuery`. Elasticsearch :
`highlight_cost` (fragment entier, balises de contrôle, reparsing compté),
`took` du moteur, cache possible sur une requête déjà vue.

## 5. La chaîne de présentation

`docs/05-09-2026/09` (la phrase, six piliers, le tableau, le plan) →
`ARCHITECTURE.md` (public, anglais, quatre propriétés en tête) → `README.md`
(trois lignes, « What's new in 4.0.0 », comparatif avec deux lignes
constatées) → `bindings/*/README.md` et `lucivy_core/README.md` (même
accroche, options nommées) → `playground/index.html` (la page). Règle :
quand le README principal change, les cinq autres suivent le même jour.

## 6. Ce qui n'a pas bougé

Le sharding, le filtre, la fédération (scores égaux à l'index unique,
`test_federated_search`), la persistance et les formats LUCE/LUCID/LUCIDS,
la requête en positions, les dérivés, la fusion : [07](07-architecture.md)
§4-8 et `docs/28-08-2026/08-architecture.md`.
