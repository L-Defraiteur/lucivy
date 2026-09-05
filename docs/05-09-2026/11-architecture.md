# Architecture de lucivy — état au 6 septembre 2026, minuit (4.0.0, branche `v4`)

Rappel écrit pour être lu seul. Il complète [07](07-architecture.md) (les
formats 4.0, la requête en positions, les dérivés et l'option, la fusion),
qui reste exact : ici ce qui a bougé depuis — le playground comme vitrine,
le banc comparatif, la chaîne de présentation — et **le chemin d'indexation
en mode dictionnaire**, où est le prochain chantier. La version anglaise
publique est `ARCHITECTURE.md` à la racine (la page que Google montre en
premier), alignée ce soir.

---

## 1. Les crates, le numéro, le contrat

Cinq crates au même numéro, **4.0.0, non publié** (3.0.8 dernière sur PyPI,
npm, crates.io). Contrat vérifié par `test_compat_308` : 4.0 ouvre un index
3.0.x et rend ce que 3.0.x rendait ; 3.0.x n'ouvre pas 4.0 ; le premier
commit convertit. Les formats : [07](07-architecture.md) §2.

## 2. L'indexation en mode dictionnaire (`sfx_version` 4) — le chemin chaud

```
document ─ tokenizer ─ SfxCollectorV3::with_dictionary(slot, field)
   pour chaque jeton distinct du segment :
     dict.lookup_or_mint(field, key, text, meta)
        ├─ field.lookup(text, meta) : pour chaque génération (≤ 8)
        │     FST get(partition + minuscules(own)) → parents où l'overlap concorde
        │     → forme égale (own_len, sep_len, word_start) → texte confirmé dans .termtexts
        └─ sinon : Mutex(state).pending[(field, key.to_string())] ou mint (next_id++)
   ordinaux locaux → ids globaux (.gmap), textes mintés → .newtexts
au commit (par shard) : génération g+1 = FST des textes nouveaux (+ union), .gmap par segment,
   compaction en flux au-delà de LUCIVY_DICT_MAX_GENERATIONS (8) : les plus petites fusionnent
```

Ce qui coûte, mesuré sur 30 000 fichiers ([10](10-journal-session-5-septembre-nuit.md)
§7) : v3 15,4 s ; dictionnaire 31,3 s ; sans compaction 29,4 (la compaction :
2 s) ; trois commits au lieu de quinze 26,8 (les générations : 4-5 s) ; le
reste, ~11 s, est le chemin par jeton ci-dessus et l'écriture de la
génération. Non instrumenté : c'est la première étape du chantier
([04](04-progression-et-a-faire.md) §2 sexies). En v3, le collecteur interne
dans une table de hachage locale et bâtit une FST par segment sur tous les
cœurs ; en dictionnaire, la génération est **par shard** (quatre en parallèle
au plus) et chaque jeton paie une recherche par génération.

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
