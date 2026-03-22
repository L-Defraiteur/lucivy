# Doc 05 — Progression : BranchNode, more_like_this, sfx optionnel

Date : 22 mars 2026
Branche : `feature/optional-sfx`

## BranchNode — nouvelle primitive luciole

Noeud conditionnel pour le DAG : évalue un bool, trigger `"then"` ou `"else"`.
Les noeuds sur le chemin inactif sont auto-skippés par le runtime (trigger
required non satisfait → propagation du skip).

```rust
dag.add_node("check", BranchNode::new(move || condition));
dag.connect("upstream", "done", "check", "trigger")?;
dag.connect("check", "then", "heavy_path", "trigger")?;
dag.connect("check", "else", "light_path", "trigger")?;
```

**Fichiers** :
- `luciole/src/branch.rs` — implémentation du BranchNode
- `luciole/src/runtime.rs` — skip des noeuds dont le trigger required n'est pas satisfait
  (ajouté aux deux chemins : séquentiel et parallèle)
- `luciole/src/lib.rs` — export `BranchNode`

**Réutilisable** : pas spécifique à lucivy, utilisable dans n'importe quel DAG luciole
(rag3weaver, pipelines de données, etc.).

## Search DAG conditionnel

Le DAG de recherche utilise maintenant un BranchNode pour skip le prescan
quand la query n'a pas besoin de SFX (term, phrase, fuzzy, regex, parse, etc.) :

```
drain → flush → needs_prescan?
                  ├── then → prescan_0..N ∥ → merge_prescan → build_weight → search_0..N ∥ → merge
                  └── else ────────────────────────────────→ build_weight → search_0..N ∥ → merge
```

BuildWeightNode a maintenant des inputs optionnels (`prescan` + `trigger`)
pour accepter les deux chemins.

### Impact sur les performances (90K docs, 4 shards)

| Query | Avant (prescan no-op) | Après (BranchNode skip) |
|-------|----------------------|------------------------|
| term 'mutex' | 0.2ms | 0.2ms |
| phrase 'mutex lock' | 1.1ms | 1.0ms |
| dismax term×2 | 9.0ms | **0.3ms** |
| more_like_this | - | **0.7ms** |
| contains 'mutex' | ~900ms | ~800ms (inchangé) |

Le gain est surtout visible sur les queries composites (dismax) qui
faisaient scheduler 4+1 prescan nodes no-op.

## Mode sfx: false

Le `SchemaConfig` accepte `sfx: false` pour skip la construction du suffix FST :

```json
{ "fields": [...], "sfx": false }
```

- `IndexSettings.sfx_enabled` persisté dans meta.json
- `SegmentWriter` : skip SfxCollector si sfx_enabled=false
- `Merger` : skip déjà naturellement si pas de .sfx dans les segments source
- `build_query` : contains/startsWith retournent erreur explicite

### Test sfx:false (4 shards, 6 docs)

```
.sfx/.sfxpost files: 0 ✓
term 'mutex': 2 hits ✓
phrase 'device drivers': 1 hits ✓
fuzzy 'mutx' d=1: 2 hits ✓
regex 'sched.*': 1 hits ✓
parse 'mutex AND lock': 1 hits ✓
phrase_prefix 'device driv': 1 hits ✓
more_like_this: 0 hits ✓ (trop peu de docs)
contains 'mutex': error ✓
startsWith 'sched': error ✓
```

## more_like_this exposé

```json
{ "type": "more_like_this", "field": "content", "value": "reference text here" }
```

Paramètres optionnels : `min_doc_frequency`, `max_doc_frequency`, `min_term_frequency`,
`max_query_terms`, `min_word_length`, `max_word_length`, `boost_factor`.

Utilise `with_document_fields` : le texte de référence est tokenisé, les termes
significatifs (haut IDF) sont extraits, une BooleanQuery(should) est construite.

- 6 docs : 0 hits (pas assez de données pour l'IDF)
- 90K docs : **20 hits en 0.7ms** ✓

## Fuzzy/Regex rebranchés sur comportement tantivy

- `"fuzzy"` → `FuzzyTermQuery` (Levenshtein sur term dict, single token)
- `"regex"` → `RegexQuery` (regex sur term dict, single token)
- Versions cross-token SFX restent via `"contains"` + distance/regex

`AutomatonWeight::collect_term_infos()` conditionné par `prefer_sfxpost` :
- `false` (fuzzy/regex top-level) → term dict standard, rapide
- `true` (contains+regex) → SFX, nécessaire pour suffixes

## Tableau complet des query types

```
Query                                 Hits       Time
-------------------------------------------------------
contains 'mutex_lock'                   20   1084.0ms
contains 'function'                     20    806.9ms
contains 'sched'                        20    731.0ms
contains 'printk'                       20    731.9ms
startsWith 'sched'                      20    777.9ms
startsWith 'printk'                     20    762.2ms
contains_split 'struct device'          20   1308.6ms
fuzzy 'schdule' (d=1)                   20    578.7ms
fuzzy 'mutex' (d=2)                     20    661.2ms
contains 'drivers' (path)               20      7.2ms
phrase 'mutex lock'                     20      1.0ms
phrase 'struct device'                  20      4.3ms
phrase 'return error'                   20      2.2ms
term 'mutex'                            20      0.2ms
phrase_prefix 'mutex loc'               20      1.1ms
phrase_prefix 'struct dev'              20      7.7ms
more_like_this 'mutex..'                20      0.7ms
dismax term×2 fields                    20      0.3ms

With highlights:
term 'mutex' +hl                        20      9.4ms
phrase 'mutex lock' +hl                 20     11.6ms
contains 'mutex' +hl                    20    733.6ms
```

## Commits branche feature/optional-sfx

```
651ff60 feat: optional SFX indexing (sfx: false in SchemaConfig)
0919862 test: verify sfx:false mode — 4-shard indexation + queries
4c7378a feat: BranchNode in luciole + conditional prescan DAG + more_like_this
```

## Prochaines étapes

- Bypass DAG complet pour queries ultra-rapides ? (term à 0.2ms = overhead DAG minimal)
- DiagBus : câbler les events manquants
- Adapter les bindings (close(), sfx flag, nouvelles queries)
- Bench vs tantivy avec fuzzy/regex (on les bat sur fuzzy 2x)
