# Doc 08 — Knowledge dump : tout ce qu'on sait (24 mars 2026)

## Commandes de test et bench

### Tests unitaires
```bash
cd packages/rag3db/extension/lucivy/ld-lucivy
cargo test --lib  # 1155 tests, ~60s
```

### Bench sharding (réutilise index persisté)
```bash
# Construire les index (une seule fois) :
BENCH_MODE="SINGLE|RR" MAX_DOCS=90000 cargo test --release -p lucivy-core --test bench_sharding bench_sharding_comparison -- --nocapture > /tmp/bench.txt 2>&1

# Query timing rapide (réutilise l'index persisté) :
find /home/luciedefraiteur/lucivy_bench_sharding -name "*.lock" -delete
cargo test --release -p lucivy-core --test bench_sharding bench_query_times -- --nocapture > /tmp/bench_query.txt 2>&1

# Ground truth exhaustif (7 termes × 4 variantes = 37 checks) :
find /home/luciedefraiteur/lucivy_bench_sharding -name "*.lock" -delete
cargo test --release -p lucivy-core --test bench_sharding ground_truth_exhaustive -- --nocapture > /tmp/ground_truth.txt 2>&1

# Score consistency single vs 4-shard :
find /home/luciedefraiteur/lucivy_bench_sharding -name "*.lock" -delete
cargo test --release -p lucivy-core --test bench_sharding test_score_consistency -- --nocapture > /tmp/score_consistency.txt 2>&1

# Test sfx:false (6 docs, 4 shards, vérifie toutes les queries) :
cargo test --release -p lucivy-core --test bench_sharding test_sfx_disabled -- --nocapture > /tmp/sfx_disabled.txt 2>&1

# Profiling regex AutomatonWeight :
cargo test --release -p lucivy-core --test bench_sharding profile_regex -- --nocapture > /tmp/profile_regex.txt 2>&1
```

### Bench vs tantivy
```bash
# Première fois : construit l'index tantivy (90K), réutilise lucivy existant
find /home/luciedefraiteur/lucivy_bench_sharding -name "*.lock" -delete
find /home/luciedefraiteur/lucivy_bench_vs_tantivy -name "*.lock" -delete
cargo test --release -p lucivy-core --test bench_vs_tantivy -- --nocapture > /tmp/bench_vs_tantivy.txt 2>&1

# Runs suivants : réutilise tous les index
# (même commande, les index sont auto-détectés)
```

### IMPORTANT : toujours rediriger vers fichier
**JAMAIS** `| tail` sur un bench. Toujours `> /tmp/fichier.txt 2>&1` puis lire le fichier.

### Index persistés
```
/home/luciedefraiteur/lucivy_bench_sharding/single/      # 90K docs, 1 shard
/home/luciedefraiteur/lucivy_bench_sharding/round_robin/  # 90K docs, 4 shards RR
/home/luciedefraiteur/lucivy_bench_vs_tantivy/tantivy/    # 90K docs tantivy 0.25
```
Les lock files doivent être supprimés avant d'ouvrir (`find ... -name "*.lock" -delete`).

## Architecture search DAG

```
drain → flush → needs_prescan?
                  ├── then → prescan_0..N ∥ → merge_prescan → build_weight → search_0..N ∥ → merge
                  └── else ────────────────────────────────→ build_weight → search_0..N ∥ → merge
```

- **DrainNode** : `pipeline.drain()` via StreamDag (readers → router → shards en topo order)
- **FlushNode** : commit les shards dirty
- **BranchNode/SwitchNode** : `needs_prescan` — skip prescan pour term/phrase/fuzzy/regex
- **PrescanShardNode** : SFX walk parallèle par shard, 1 noeud par shard
- **MergePrescanNode** : agrège caches + doc_freqs
- **BuildWeightNode** : reçoit query pré-construite + prescan results, compile Weight
- **SearchShardNode** : exécute le Weight sur un shard via shard pool
- **MergeResultsNode** : binary heap merge top-K

La query est construite AVANT le DAG (`build_query` dans `build_search_dag`).
Pas de compilation DFA/regex dans le DAG.

## Noeuds luciole disponibles

| Noeud | Fichier | Description |
|-------|---------|-------------|
| Node (trait) | `luciole/src/node.rs` | execute, can_undo, undo, undo_context, node_config |
| PollNode (trait) | `luciole/src/node.rs` | exécution coopérative (WASM) |
| SwitchNode | `luciole/src/branch.rs` | N-way routing conditionnel |
| BranchNode | `luciole/src/branch.rs` | alias 2-way (fn, pas struct) |
| GateNode | `luciole/src/gate.rs` | pass/block conditionnel |
| MergeNode | `luciole/src/fan_out.rs` | N inputs → 1 output, custom merge fn |
| fan_out_merge() | `luciole/src/fan_out.rs` | helper Dag : N workers + merge |
| ScatterDAG | `luciole/src/scatter.rs` | closures parallèles |
| StreamDag | `luciole/src/stream_dag.rs` | pipeline streaming (actors) |
| CheckpointStore | `luciole/src/checkpoint.rs` | crash recovery |
| TapRegistry | `luciole/src/observe.rs` | observabilité edges |
| Pool\<M\> | `luciole/src/pool.rs` | pool d'acteurs typés |
| Scope | `luciole/src/scope.rs` | drain structuré |
| ServiceRegistry | `luciole/src/node.rs` | services partagés dans NodeContext |

### BranchNode usage (attention : c'est une FONCTION pas un struct)
```rust
dag.add_node("check", luciole::BranchNode(move || condition));
// PAS: luciole::BranchNode::new(...)
```

### fan_out_merge usage
```rust
dag.fan_out_merge::<ResultType>("prefix", count,
    |i| Box::new(WorkerNode::new(i)),
    "output_port",
    |results| merge_fn(results),
)?;
// Crée prefix_0..N + prefix_merge
```

### Dag avec services
```rust
let mut services = ServiceRegistry::new();
services.register("conn", my_connection);
let dag = Dag::new().with_services(Arc::new(services));
// Dans un node : ctx.service::<MyConn>("conn")
```

### add_node_boxed (pour Box<dyn Node>)
```rust
dag.add_node_boxed("name", factory_fn(i));  // prend Box<dyn Node>
dag.add_node("name", concrete_node);        // prend impl Node
```

## BM25 / Scoring

### EnableScoring — Arc<dyn Bm25StatisticsProvider>
```rust
EnableScoring::Enabled {
    searcher: &Searcher,                                    // pour schema, tokenizers
    stats: Arc<dyn Bm25StatisticsProvider + Send + Sync>,   // pour IDF global
}
```

- `enabled_from_searcher(searcher)` : clone Searcher dans Arc (single shard)
- `enabled_from_statistics_provider(Arc, searcher)` : multi-shard ou distributed
- Plus de `Copy` — c'est `Clone` (Arc clone = cheap)

### Score consistency 5/5
```
term:   single=5.1066  4sh=5.1066  diff=0.0000
phrase: single=3.0194  4sh=3.0194  diff=0.0000
fuzzy:  single=9.2734  4sh=9.2734  diff=0.0000
regex:  single=130.719 4sh=130.719 diff=0.0000
parse:  single=8.5633  4sh=8.5633  diff=0.0000
```

### Queries et leur IDF source

| Query | IDF | sfx:false OK |
|-------|-----|-------------|
| term | EnableScoring.stats (global) | oui |
| phrase | EnableScoring.stats (global) | oui |
| fuzzy (top-level) | AutomatonWeight.stats + global_doc_freq() | oui |
| regex (top-level) | AutomatonWeight.stats + global_doc_freq() | oui |
| TermSet | AutomatonWeight.stats | oui |
| parse | délègue term/phrase | oui |
| phrase_prefix | délègue phrase | oui |
| disjunction_max | délègue sous-queries | oui |
| more_like_this | stats_provider param | oui |
| boolean | délègue sous-queries | oui |
| contains | prescan global_doc_freq | NON (erreur) |
| startsWith | prescan | NON (erreur) |

### AutomatonWeight — global doc_freq
`collect_term_infos` retourne `Vec<(Vec<u8>, TermInfo)>`.
`global_doc_freq(term_bytes, local_df)` reconstruit un Term et appelle `stats.doc_freq()`.
Coût : ~0 pour single shard (stats=None → skip), ~N lookups pour multi-shard.

### sfx_prescan_params() — single source of truth
La query construite expose ses params prescan via le trait Query.
Plus de `extract_contains_terms()` — c'est la query elle-même qui dit ce qu'elle a besoin.

## Performances (90K docs Linux kernel, 4 shards RR)

### vs tantivy 0.25
```
                              Tantivy   Lucivy-1   Lucivy-4
term 'mutex'                   0.2ms      0.2ms      0.3ms
phrase 'struct device'        10.7ms     10.5ms      4.2ms    ← 2.5x faster
fuzzy 'mutex' d=2             18.3ms     16.7ms     10.6ms    ← 2x faster
regex 'mutex.*'                0.3ms      0.3ms      0.8ms    ← DAG overhead
parse '"return error"'         4.8ms      4.5ms      2.0ms    ← 2.4x faster
```

### Lucivy-only
```
contains 'mutex_lock'         ~900ms (4sh)    ~2500ms (1sh)
startsWith 'sched'            ~700ms (4sh)    ~2000ms (1sh)
phrase_prefix 'mutex loc'       1ms (4sh)
more_like_this                  0.7ms (4sh)
```

### Bench important : build_query est HORS timer
Tantivy construit le RegexQuery AVANT le timer. On fait pareil dans
`time_lucivy_single`. Pour `time_lucivy_sharded` (DAG), build_query est
dans `build_search_dag` AVANT le DAG.

## Fichiers clés modifiés cette session

| Fichier | Changement principal |
|---------|---------------------|
| `src/query/query.rs` | EnableScoring Arc + SfxPrescanParam + sfx_prescan_params + require_sfx |
| `src/query/automaton_weight.rs` | stats: Option<Arc>, global_doc_freq(), bm25_stats(), collect_term_infos retourne (bytes, TermInfo) |
| `src/query/term_query/term_weight.rs` | Suppression SFX fallback, utilise term dict standard |
| `src/query/fuzzy_query.rs` | with_stats(Arc) au lieu de with_global_stats |
| `src/query/regex_query.rs` | idem |
| `src/query/set_query.rs` | idem |
| `src/query/more_like_this/` | stats_provider param pour IDF global |
| `src/query/phrase_query/suffix_contains_query.rs` | run_sfx_walk + segment_id, DiagBus events |
| `src/query/phrase_query/regex_phrase_weight.rs` | prefer_sfxpost=true pour contains+regex |
| `src/index/index_meta.rs` | sfx_enabled dans IndexSettings |
| `src/indexer/segment_writer.rs` | skip SfxCollector si sfx_enabled=false |
| `src/core/searcher.rs` | search_with_statistics_provider prend Arc |
| `luciole/src/branch.rs` | SwitchNode (N-way) + BranchNode (fn alias) |
| `luciole/src/gate.rs` | GateNode |
| `luciole/src/fan_out.rs` | MergeNode + Dag::fan_out_merge() |
| `luciole/src/dag.rs` | services, add_node_boxed, with_services |
| `luciole/src/node.rs` | ServiceRegistry dans NodeContext, node_config() |
| `luciole/src/runtime.rs` | rollback dans execute_dag_with_checkpoint, services propagation |
| `luciole/src/mailbox.rs` | Drainable for ActorRef |
| `lucivy_core/src/query.rs` | build_query: sfx flag, phrase_prefix, disjunction_max, more_like_this, fuzzy/regex rebranchés tantivy |
| `lucivy_core/src/search_dag.rs` | BranchNode, pre-built query, StreamDag drain, prescan params from query |
| `lucivy_core/src/sharded_handle.rs` | StreamDag pipeline, build_pipeline() |
| `lucivy_core/src/bm25_global.rs` | AggregatedBm25StatsOwned impl Bm25StatisticsProvider |
| `lucivy_core/benches/bench_sharding.rs` | ground_truth_exhaustive, score_consistency, sfx_disabled, profile_regex, bench_query_times |
| `lucivy_core/benches/bench_vs_tantivy.rs` | fair bench (build_query hors timer), fuzzy/regex tantivy |

## Branches

- `feature/acid-postgres-tests` : vtable fix, DAG prescan, ground truth, bench
- `feature/optional-sfx` : sfx:false, BranchNode, convergence luciole, StreamDag

## Bugs corrigés (à retenir)

- **sed casse les fichiers** : JAMAIS utiliser sed sur les fichiers Rust, utiliser Edit
- **Box<dyn Query> vtable** : les méthodes default du trait ne sont PAS déléguées automatiquement. Il faut les overrider dans `impl Query for Box<dyn Query>`
- **EnableScoring plus Copy** : les closures et multi-usage nécessitent `.clone()` ou sauver le bool avant le move
- **Lock files** : les bench laissent des `.lucivy-writer.lock` — les supprimer avant de ré-ouvrir
- **continuation=true hardcodé** : l'ancien `extract_contains_terms` forçait continuation=true alors que la query ne l'utilisait pas → 10-50x plus lent
- **TermQuery prefer_sfxpost** : ouvrait le SFX file pour un simple term lookup → 1000ms au lieu de 0.2ms
- **AutomatonWeight SFX fallback** : ouvrait le SFX pour fuzzy/regex alors que le term dict suffit → 7ms au lieu de 0.2ms
- **Bench unfair** : tantivy met build_query HORS timer, nous le mettions DANS → faux retard de 0.3ms
