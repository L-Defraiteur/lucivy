# Doc 01 — État des lieux et roadmap

Date : 24 mars 2026
Branches : `feature/acid-postgres-tests`, `feature/optional-sfx`

## Ce qui a été fait (sessions 22-24 mars)

### BM25 / Scoring
- **Vtable fix** `Box<dyn Query>` : toutes les prescan methods dispatchent correctement
- **Arc<dyn Bm25StatisticsProvider>** dans `EnableScoring` : stats globales partageables
- **AutomatonWeight** : utilise `stats.doc_freq(term)` global pour chaque terme matché
- **MoreLikeThisQuery** : utilise le stats provider global au lieu de `searcher_0`
- **Score consistency** : 5/5 queries identiques single vs 4-shard (diff=0.0000)

### DAG / Architecture
- **Prescan en noeuds DAG parallèles** (pas nested scatter)
- **BranchNode** dans luciole : routing conditionnel `then`/`else`
- **Search DAG conditionnel** : skip prescan pour queries sans SFX
- **Query pré-construite** : build_query avant le DAG (sauve la compilation DFA/regex)
- **sfx_prescan_params()** : single source of truth pour les paramètres prescan

### Performances
- **term** : 1004ms → 0.2ms (suppression sfxpost, suppression SFX fallback)
- **phrase** : 2-3x plus rapide que tantivy (parallélisme 4 shards)
- **fuzzy** : 2x plus rapide que tantivy
- **regex** : égal tantivy (0.3ms single, 0.8ms 4-shard — overhead DAG)
- **contains/startsWith** : exclusif lucivy, ~700ms sur 90K

### Features
- **sfx: false** dans SchemaConfig (indexation sans SFX)
- **phrase_prefix**, **disjunction_max**, **more_like_this** exposés
- **fuzzy/regex** rebranchés sur comportement tantivy standard
- **DiagBus** : SearchMatch + SearchComplete câblés dans `run_sfx_walk`
- **Ground truth** : 37/37 sur 90K, 5/5 score consistency

### Bench vs tantivy 0.25 (90K docs Linux kernel)

```
                              Tantivy   Lucivy-1   Lucivy-4
term                           0.2ms      0.2ms      0.3ms
phrase 'struct device'        10.7ms     10.5ms      4.2ms
fuzzy 'mutex' d=2             18.3ms     16.7ms     10.6ms
regex 'mutex.*'                0.3ms      0.3ms      0.8ms
parse '"return error"'         4.8ms      4.5ms      2.0ms
contains (lucivy only)           —     2100ms      700ms
```

## Ce qu'il reste à faire

### Luciole — framework DAG/Actor
1. **SwitchNode** : généralisation du BranchNode (N outputs)
2. **FanOutMerge** : pattern parallélisation + agrégation en un appel
3. **GateNode** : pass/block conditionnel
4. **LoopNode / RetryNode** : itération / retry
5. **TimeoutNode** : wrapper avec timeout
6. **README + exemples** pour publication standalone

### Lucivy — moteur search
7. **DiagBus** events restants : SfxWalk, SfxResolve, SuffixAdded, MergeDocRemapped
8. **Adapter les bindings** : close(), sfx flag, nouvelles queries (5 bindings)
9. **Réduire overhead DAG** pour queries ultra-rapides (<1ms)
10. **MoreLikeThisQuery** score consistency multi-shard à valider

### Infrastructure
11. **CI/CD** : tests automatisés, bench regression
12. **Publication** crates.io : luciole + lucivy-core
13. **Documentation** API publique
