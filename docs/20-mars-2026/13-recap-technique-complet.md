# Doc 13 — Recap technique complet de la session

Date : 20 mars 2026
Branche : `feature/luciole-dag`
Résultat : 5/5 MATCH sur 90K, 1200 tests, 0 fail

## Luciole — le framework de coordination

Crate séparé dans `luciole/`. Pool de threads persistants unifié.

### Concepts clés
- **Node** : trait `execute(&mut self, ctx: &mut NodeContext) → Result<(), String>`
- **PollNode** : trait pour yield coopératif (non utilisé après refacto — tout est Node maintenant)
- **Dag** : graphe de nodes avec ports typés. `connect(from, port, to, port)` vérifie les types.
- **PortValue** : `Arc<dyn Any + Send + Sync>`. `take::<T>()` panic sur fan-out (détecte multi-ref).
- **PortType** : `Typed(TypeId)`, `Trigger`, `Any`. Vérifié au connect.
- **execute_dag** : topological sort par niveaux, parallèle intra-niveau.
  Si appelé depuis un scheduler thread → exécution inline (anti-deadlock).
- **DagResult** : `take_output::<T>(node, port)`, `display_summary()`, métriques par node.
- **GraphNode** : sub-DAG comme un seul node. `set_initial_input()` + `take_output()`.
- **StreamDag** : topologie pour drain ordering des acteurs.
- **Scheduler** : pool de threads persistants. `submit_task(priority, closure)`.
  `is_scheduler_thread()` via thread-local pour détecter les threads du pool.
- **Pool<M>** : N acteurs typés, round-robin ou key-routed. `drain()`, `scatter()`.
- **Scope** : lifecycle manager. `add()`, `drain()`, `execute_dag()`.
- **ScatterDag** : `build_scatter_dag(vec![("name", closure), ...])` → N tâches parallèles,
  `CollectNode` produit `HashMap<String, PortValue>`, `ScatterResults::take::<T>("name")`.
- **subscribe_dag_events()** : `DagEvent` bus (DagCompleted, NodeCompleted, NodeFailed, etc.)
- **TapRegistry** : edge tapping pour inspecter les données entre nodes.
- **CheckpointStore** : persistence + recovery (MemoryCheckpointStore, FileCheckpointStore).

### Fichiers
```
luciole/src/
  dag.rs, runtime.rs, node.rs, port.rs, pool.rs, scope.rs,
  graph_node.rs, stream_dag.rs, scheduler.rs, mailbox.rs,
  observe.rs, checkpoint.rs, events.rs, scatter.rs, lib.rs
```

## Architecture DAG de lucivy

### Commit DAG (`commit_dag.rs`)
```
prepare ──┬── merge_0 ──┐
          ├── merge_1 ──┼── finalize ── save ── gc ── reload
          └── merge_2 ──┘
```
- **PrepareNode** : `purge_deletes()` + `segment_manager.commit()` + `start_merge()` par op
- **MergeNode** : exécute le merge_dag inline (voir ci-dessous)
- **FinalizeNode** : `end_merge()` pour chaque résultat, advance deletes
- **SaveMetasNode** : écriture atomique meta.json
- **GCNode** : `garbage_collect_files()` (whitelist approach)
- **ReloadNode** : no-op (reader lit meta.json au prochain search)
- **Cascade loop** : `handle_commit()` boucle jusqu'à plus de merge candidates

### Merge DAG (`merge_dag.rs`)
```
init ──┬── postings ──────────┐
       ├── store ─────────────┼── sfx ── close
       └── fast_fields ───────┘
```
- **InitNode** : `SegmentSerializer::decompose()` → writers indépendants.
  Clone `doc_id_mapping` en deux outputs (postings + fast_fields) pour éviter fan-out.
- **PostingsNode** : écrit les postings pour tous les champs indexés
- **StoreNode** : copie le document store
- **FastFieldsNode** : écrit les colonnes fast
- **SfxNode** : exécute le sfx_dag par champ text
- **CloseNode** : ferme tous les writers, construit `SegmentEntry`

### SFX DAG (`sfx_dag.rs`)
```
collect_tokens ──┬── build_fst ─────────────────────────┐
                 ├── copy_gapmap ── validate_gapmap ─────┼── write_sfx
                 └── merge_sfxpost ── validate_sfxpost ──┘
```
- Construit le suffix FST + gapmap + sfxpost pour UN champ
- `WriteSfxNode` écrit via `Segment::open_write_custom()` (pas de SegmentSerializer)
- Imbriqué dans SfxNode du merge_dag : 3 niveaux de DAG (commit > merge > sfx)

### Search DAG (`search_dag.rs` dans lucivy_core)
```
drain → flush → build_weight → search_shard_0..N ∥ → merge_results
```

### Scatter DAG (`scatter.rs` dans luciole)
- Index opening : `seg_0`, `seg_1`, ... → `CollectNode` → `ScatterResults`
- SFX build dans finalize : `field_0`, `field_1`, ... → `CollectNode`

## Le Suffix FST (SFX)

### Principe
Pour chaque token du term dict, génère TOUS les suffixes :
- "function" → "function"(SI=0), "unction"(SI=1), ..., "n"(SI=7)
- Stockés dans un FST avec prefix byte : `0x00` pour SI=0, `0x01` pour SI>0
- `prefix_walk("lock")` trouve tous les tokens contenant "lock" comme substring
- `prefix_walk_si0("lock")` ne trouve que les tokens commençant par "lock"

### Fichiers par segment
- `.{field_id}.sfx` : suffix FST + parent list + gapmap
- `.{field_id}.sfxpost` : postings par ordinal (doc_id, token_index, byte_from, byte_to)
- Référencés dans `SegmentMeta.sfx_field_ids` (persisté dans meta.json)

### Parent encoding
Chaque suffix dans le FST a un ou plusieurs parents (tokens dont il est le suffixe) :
- **Single parent** : encodé directement dans la valeur u64 du FST (raw_ordinal + si)
- **Multi parent** : offset dans l'OutputTable, record = `[num_parents: u16] + [raw_ordinal: u32, si: u16]*`
- **BUG FIXÉ** : le num_parents était `u8` → max 255. Passé à `u16` (fix du DIFF=663 sur "lock")

### GapMap
Stocke les séparateurs inter-tokens par document. Format per-doc :
- Header : (num_tokens, num_values)
- Gaps encodés (bytes entre tokens consécutifs)
- Utilisé par le multi-token search et la continuation pour vérifier que le gap est vide

### SfxCollector (`suffix_fst/collector.rs`)
- Reçoit les tokens via `SfxTokenInterceptor` pendant l'indexation BM25
- Stocke gapmaps + postings (doc_id, token_index, byte_from, byte_to)
- `build()` : trie les tokens, construit le FST + gapmap + sfxpost

## Le tokenizer RAW_TOKENIZER

`SimpleTokenizer → CamelCaseSplitFilter → LowerCaser`

### CamelCaseSplitFilter (fixé dans cette session)
Règles de split :
- **lower→UPPER** : `getElement` → `get` | `Element`
- **UPPER→UPPER+lower** : `HTMLParser` → `HTML` | `Parser`
- **letter↔digit** : `var123` → `var` | `123`
- **PAS de split** : ALL_CAPS (`FUNCTION`, `SCHEDULER` restent un seul token)
- Merge les chunks < 4 chars avec le suivant
- Force-split les chunks > 256 bytes

### SfxTokenInterceptor (`suffix_fst/interceptor.rs`)
Wrap le TokenStream du BM25 indexing. Capture les tokens pour le SfxCollector
sans re-tokeniser. Un seul passage de tokenisation (stemming supprimé).

## Contains search — le chemin complet

### Single-token path (`suffix_contains.rs`)
1. `prefix_walk(query)` sur le suffix FST (les deux partitions SI=0 et SI>0)
2. Pour chaque (suffix, parents) : `resolver(raw_ordinal)` → posting entries
3. Match : `(doc_id, byte_from + si, byte_from + si + query_len)`
4. Déduplication par (doc_id, byte_from)

### Cross-token expansion (uppercase)
Si query single-token : on tokenise aussi `query.to_uppercase()` avec le RAW_TOKENIZER.
Si ça donne plusieurs tokens → multi-token search avec gaps vides.
Couvre les cas où le doc a le même split que la query uppercase.

### Cross-token continuation (hybride)
Quand le walk 1 détecte un match partiel (query dépasse la fin du token) :

**Source 1** : walk 1 entries où `si + query_len > token_byte_len`
  → consumed = `token_len - si`, remaining = `query[consumed..]`

**Source 2** : tokens qui FINISSENT par un préfixe du query
  → `prefix_walk(query[..k])` filtré par `si + k == token_len`

Pour chaque candidat : gapmap vide → `prefix_walk_si0(remaining)` → join sur (doc_id, ti+1).
Boucle jusqu'à depth 8 pour les splits en N tokens.

### Multi-token path (`suffix_contains_multi_token`)
Pour les queries multi-mots (ex: "struct device") :
1. Tokenise la query → ["struct", "device"] avec séparateur " "
2. Walk par token : premier token any-SI, suivants SI=0 only, dernier prefix
3. Join sur positions consécutives + validation gaps via GapMap

## DiagBus (`src/diag.rs`)

### Architecture
```rust
static DIAG_BUS: OnceLock<DiagBus>
static VERBOSE: AtomicBool
```
- `diag_bus().subscribe(DiagFilter)` → `Receiver<DiagEvent>` (unbounded channel)
- `diag_bus().emit(event)` → dispatche aux subscribers matchants
- `is_active()` : atomic bool check, 1ns si pas de subscribers
- `set_verbose(false)` : coupe les eprintln DAG summaries

### Events
- `TokenCaptured { doc_id, field_id, token, offset_from, offset_to }`
- `SearchMatch { query, segment_id, doc_id, byte_from, byte_to, cross_token }`
- `SearchComplete { query, segment_id, total_docs }`
- `SuffixAdded`, `SfxWalk`, `SfxResolve`, `MergeDocRemapped`

### Filtres
- `All`, `Tokenization`, `Sfx`, `SfxTerm("lock")`, `Merge`

### trace_search (`lucivy_core/src/diagnostics.rs`)
Diagnostic structuré "pourquoi doc X est/n'est pas trouvé par contains Y" :
1. Ground truth : `text.to_lowercase().contains(query)` sur le stored doc
2. Tokenizer : quels tokens contiennent le query
3. Term dict : le token est-il dans le BM25 postings pour ce doc
4. Suffix FST : prefix_walk → ordinals, parents
5. Sfxpost : le doc est-il dans les posting entries pour chaque ordinal
**Lit les bytes directement** (pas de build_resolver) → zéro deadlock.

## Pipeline commit/merge (après redesign)

### SegmentUpdaterState (simplifié)
```rust
struct SegmentUpdaterState {
    shared: Arc<SegmentUpdaterShared>,
    // Plus de active_merge, explicit_merge, pending_merges, segments_in_merge
}
```

### handle_commit (cascade loop)
```rust
fn handle_commit(&mut self, opstamp, payload) {
    loop {
        let candidates = self.collect_merge_candidates(); // pool unifié
        let no_merges = candidates.is_empty();
        let dag = build_commit_dag(shared, candidates, opstamp, payload);
        execute_dag(dag);
        if no_merges { break; }
    }
}
```

### handle_merge (explicit merge API)
```rust
fn handle_merge(&mut self, merge_operation) {
    let dag = build_commit_dag(shared, vec![merge_operation], ...);
    execute_dag(dag);
}
```

### wait_merging_threads : no-op (merges sont synchrones dans le DAG)

### Merge candidates : pool unifié (committed + uncommitted ensemble)

## SegmentComponent

### Variants natifs pour SFX
```rust
enum SegmentComponent {
    Postings, Positions, FastFields, FieldNorms, Terms, Store, Offsets,
    SuffixFst { field_id: u32 },    // .{field_id}.sfx
    SuffixPost { field_id: u32 },   // .{field_id}.sfxpost
}
```

### sfx_field_ids
- `InnerSegmentMeta.sfx_field_ids: Vec<u32>` — persisté dans meta.json
- `list_files()` inclut automatiquement les per-field .sfx/.sfxpost
- Le GC protège nativement les fichiers per-field (plus de hack gc_protected_segments)

## Benchmarks

### Dataset
Linux kernel source : ~91K fichiers C/H dans `/home/luciedefraiteur/linux_bench`.

### Structure du bench (`lucivy_core/benches/bench_sharding.rs`)
1. Index single shard → 1-shard timing
2. Index 4 shards token-aware (balance_weight=0.2) → TA timing
3. Index 4 shards round-robin (balance_weight=1.0) → RR timing
4. Queries comparatives sur les 3 modes (top-20)
5. Post-mortem : inspect_term avec ground truth verification
6. Real search vs ground truth via DiagBus (SearchComplete events)
7. Missing docs trace (trace_search sur les docs manquants)

### Ground truth
`LUCIVY_VERIFY=1` active la vérification :
- Itère tous les stored docs
- `text.to_lowercase().contains(term)`
- Compare avec le count du search collector

### Index préservé
`/home/luciedefraiteur/lucivy_bench_sharding/` (single/, token_aware/, round_robin/)
Peut être réouvert directement pour investigation sans ré-indexation :
```rust
let dir = StdFsDirectory::open("path/to/shard_N").unwrap();
let handle = LucivyHandle::open(dir).unwrap();
```
Attention aux lock files (`.lucivy-writer.lock`) — supprimer avant de réouvrir.

### Résultats 90K (20 mars 2026)
```
mutex:     8850/ 8850  MATCH
lock:     40389/40389  MATCH
function: 21525/21525  MATCH
printk:    4681/ 4681  MATCH
sched:     8945/ 8945  MATCH
```

## Build commands

```bash
# Tests ld-lucivy (1200 tests)
cd packages/rag3db/extension/lucivy/ld-lucivy && cargo test --lib

# Tests luciole (132+ tests)
cd luciole && cargo test

# Tests lucivy_core (83+ tests)
cd lucivy_core && cargo test

# Bench 5K (rapide, ~30s)
MAX_DOCS=5000 LUCIVY_VERIFY=1 cargo test --release --package lucivy-core --test bench_sharding -- --nocapture

# Bench 90K (complet, ~25min avec ground truth)
MAX_DOCS=90000 LUCIVY_VERIFY=1 cargo test --release --package lucivy-core --test bench_sharding -- --nocapture

# Investigation sur index persisté
cargo test --package lucivy-core --test test_lock_investigation -- --nocapture
```
