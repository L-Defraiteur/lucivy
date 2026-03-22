# Doc 01 — Recap session : DAG prescan, vtable fix, ground truth exhaustif

Date : 22 mars 2026
Branche : `feature/acid-postgres-tests`

## Bugs identifiés et corrigés

### 1. Vtable dispatch cassé sur `Box<dyn Query>`

`impl Query for Box<dyn Query>` ne déléguait que `weight()`, `count()`, `query_terms()`.
Les 5 méthodes prescan (`set_global_contains_doc_freqs`, `inject_prescan_cache`, etc.)
tombaient sur le **default no-op du trait** → le cache prescan était calculé puis jeté.

**Fix** : ajout des 6 délégations manquantes (5 prescan + `sfx_prescan_params`).

### 2. `extract_contains_terms` : continuation=true hardcodé

La fonction `extract_contains_terms` forçait `continuation=true` pour tous les `contains`,
alors que `build_contains_query` ne met PAS continuation. Le prescan utilisait
`suffix_contains_single_token_continuation` (DFA cross-token, 10-50x plus cher)
alors que le scorer utilisait `suffix_contains_single_token` (simple).

**Fix** : suppression de `extract_contains_terms`. Remplacé par `Query::sfx_prescan_params()`
qui retourne les paramètres exacts de la query construite — single source of truth.

### 3. Prescan niché dans BuildWeightNode (nested DAG)

Le prescan scatter DAG tournait DANS BuildWeightNode (nested `execute_dag` dans un noeud DAG).
Potentielle sérialisation + un seul slot DAG occupé.

**Fix** : prescan extrait en noeuds DAG first-class.

## Architecture DAG actuelle

```
drain → flush → prescan_0..N ∥ → merge_prescan → build_weight → search_0..N ∥ → merge
```

- **PrescanShardNode** (×N) : SFX walk sur un shard, paramètres issus de `sfx_prescan_params()`
- **MergePrescanNode** : agrège caches + doc_freqs
- **BuildWeightNode** : construit la query, injecte cache + freqs, compile Weight
- **SearchShardNode** (×N) : scorer depuis cache (0ms), via shard pool
- **MergeResultsNode** : binary heap merge top-K

Pour les query sans SFX (term, phrase, regex, fuzzy) : les prescan nodes sont no-op (0ms).

## Trait `Query` — méthodes ajoutées

```rust
trait Query {
    // ... existant ...
    fn prescan_segments(&mut self, segments: &[&SegmentReader]) -> Result<()>;
    fn collect_prescan_doc_freqs(&self, out: &mut HashMap<String, u64>);
    fn set_global_contains_doc_freqs(&mut self, freqs: &HashMap<String, u64>);
    fn take_prescan_cache(&mut self, out: &mut HashMap<SegmentId, CachedSfxResult>);
    fn inject_prescan_cache(&mut self, cache: HashMap<SegmentId, CachedSfxResult>);
    fn sfx_prescan_params(&self) -> Vec<SfxPrescanParam>;  // NEW
}
```

Implémenté par : `SuffixContainsQuery`, `BooleanQuery` (propage).
Délégué par : `Box<dyn Query>` (vtable fix).

## Performances 5K (Linux kernel, 4 shards RR)

```
contains 'mutex_lock'           80ms
contains 'function'             84ms
contains 'sched'                84ms
contains_split 'struct device'  85ms
startsWith 'sched'              82ms
fuzzy 'schdule' (d=1)          100ms
```

Avant fix : `function` 750ms, `struct device` 3532ms, `sched` 2135ms.

## Performances 90K (Linux kernel, 4 shards RR)

```
contains 'mutex_lock'          1011ms
contains 'function'             774ms
contains 'sched'                790ms
startsWith 'sched'              916ms
contains_split 'struct device' 1715ms
fuzzy 'schdule' (d=1)           828ms
fuzzy 'mutex' (d=2)            1099ms
phrase 'mutex lock'               1ms
phrase 'struct device'            5ms
term 'mutex'                   1004ms  ← prefer_sfxpost=true, à optimiser
contains 'drivers' (path)       10ms
```

## Ground truth exhaustif — 37/37 pass

Test `ground_truth_exhaustive` sur l'index 90K persisté :
- 7 termes × 4 variantes (contains, startsWith, fuzzy d=1, fuzzy d=2)
- 2 contains_split phrases
- **contains** : 14/14 MATCH exact (substring scan vs search count)
- **startsWith** : 7/7 OK (search ≥ ground truth, diff = tokenisation)
- **fuzzy** : 14/14 monotone (d=2 ≥ d=1 ≥ exact)
- **contains_split** : 2/2 OK (search ≥ AND ground truth, OR semantics)
- Chaque variante affiche top-3 highlights avec score + contexte

```
search "mutex":    8850 docs | ground_truth=8850  ✓ MATCH
search "lock":    40389 docs | ground_truth=40389 ✓ MATCH
search "function":21525 docs | ground_truth=21525 ✓ MATCH
search "printk":   4681 docs | ground_truth=4681  ✓ MATCH
search "sched":    8945 docs | ground_truth=8945  ✓ MATCH
```

## DiagBus — état

5 des 6 events ne sont jamais émis (les `emit()` n'ont jamais été câblés).
Seul `TokenCaptured` fonctionne (segment_writer.rs).
Doc 27 (dossier 20-mars-2026) détaille le plan de réparation.

Le bench ground truth a été corrigé pour utiliser `Count` collector directement
au lieu de compter les `SearchMatch` events du DiagBus.

## Fichiers modifiés

| Fichier | Changement |
|---------|-----------|
| `src/query/query.rs` | Vtable fix + `SfxPrescanParam` struct + `sfx_prescan_params()` |
| `src/query/mod.rs` | Re-export `SfxPrescanParam` |
| `src/query/phrase_query/suffix_contains_query.rs` | Impl `sfx_prescan_params()` |
| `src/query/boolean_query/boolean_query.rs` | Impl `sfx_prescan_params()` (propage) |
| `lucivy_core/src/search_dag.rs` | PrescanShardNode, MergePrescanNode, suppression extract_contains_terms |
| `lucivy_core/benches/bench_sharding.rs` | Ground truth exhaustif + bench_query_times + snippets |
| `docs/20-mars-2026/27-diagbus-etat-et-plan-repair.md` | Plan DiagBus |

## Commits

```
80a40be feat: parallel prescan DAG nodes + vtable fix + sfx_prescan_params
1f00973 test: exhaustive ground truth with highlight snippets
```
