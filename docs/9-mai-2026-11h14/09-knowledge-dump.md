# Knowledge dump — 9 mai 2026

## Concepts clés à connaître

### SFX Engine — le différenciateur

Le SFX (Suffix FST) est l'algo propriétaire de lucivy. Pour chaque token
indexé, on génère toutes les suffixes et on les stocke dans un FST partitionné.

**Partitionnement SI** :
- **SI=0** : le suffix commence au début du token ("programming" → SI=0)
- **SI=3** : le suffix commence au 3e byte ("gramming" → SI=3)

Le FST est partitionné en deux branches (prefixed by `0` ou `1`) :
- `prefix_walk_si0(query)` : ne cherche que dans SI=0 (pour startsWith/anchor_start)
- `prefix_walk(query)` : cherche dans tous les SI (pour contains)

**Cross-token matching** :
Quand un query ("rag3weaver") est tokenisé en ["rag3", "weaver"] par le
RAW_TOKENIZER, le `falling_walk` + `sibling_table` reconstruit le match
en traversant les frontières de tokens.

Le `falling_walk` descend le FST tant que le query matche, et quand il
ne peut plus avancer (fin du token indexé), il utilise la `sibling_table`
pour sauter au token suivant et continuer le match.

**Fichiers par segment** :
- `.sfx` : Suffix FST (toutes les suffixes, partitionné)
- `.sfxpost` : posting lists suffix_ordinal → doc_ids (v2 binary)
- `.termtexts` : texte des tokens (ordinal → string, pour sibling chain)
- `.gapmap` : séquences bytes gap-encoded (pour séparateurs)
- `.sepmap` : carte des séparateurs entre tokens

### Fuzzy — trigram pigeonhole

Le fuzzy (distance > 0) utilise `RegexContinuationQuery` :
1. Extraire les littéraux du pattern
2. Pour chaque littéral, générer des trigrams
3. Au moins un trigram doit matcher exactement (pigeonhole principle)
4. Chercher ce trigram dans le SFX → candidats
5. Valider le match complet avec Levenshtein DFA

**Bug connu** : les character classes `[a-z]+` ne fonctionnent pas dans
la validation post-SFX. `program.+` marche, `program[a-z]+` non.

### Regex — extraction de littéraux

Le regex cross-token utilise aussi `RegexContinuationQuery` :
1. `extract_literals_with_gaps(pattern)` extrait les parties littérales
2. Les littéraux sont cherchés via SFX (rapide)
3. Les gaps (parties non-littérales) sont validés par regex sur les candidats
4. Pas de scan complet de l'index

### BM25 scoring cross-shard

`EnableScoring::Enabled(Arc<dyn Bm25StatisticsProvider>)` porte les stats
globales. Chaque Weight utilise ces stats pour le calcul IDF.

Pour le distribué :
```
Node A: export_stats(query) → ExportableStats (JSON sérialisable)
Node B: export_stats(query) → ExportableStats
Coordinator: ExportableStats::merge([A, B]) → global_stats
Node A: search_with_global_stats(query, top_k, global_stats)
Node B: search_with_global_stats(query, top_k, global_stats)
Coordinator: merge top-K results
```

`ExportableStats` contient :
- `total_num_docs` : total docs sur ce node
- `total_num_tokens` : par field_id
- `doc_freqs` : par terme sérialisé
- `contains_doc_freqs` : par query text SFX (du prescan)
- `regex_doc_freqs` : par regex pattern

### Scoring fuzzy — tiers négatifs

Le scoring fuzzy utilise un système de tiers :
```
score = miss_penalty * 1000 + bm25_score
```
- 0 misses → score positif (BM25 pur, exact match tier)
- 1 miss → -1000 + BM25 (1-edit tier)
- 2 misses → -2000 + BM25

Les scores négatifs sont **voulus** — l'ordre est correct (exact > 1-edit > 2-edit),
le BM25 départage dans chaque tier.

### Sharding — routing hybride

`ShardRouter` route les documents par scoring hybride :
```
score = (1 - balance_weight) × per_token_score + balance_weight × shard_load_ratio
```
- `balance_weight=1.0` (default) : round-robin, indexation rapide
- `balance_weight=0.2` : token-aware, co-localise les documents similaires

Le router maintient des compteurs par shard par token (HashMap<token_hash, count>).
Seuls les tokens avec df < `df_threshold` (default 5000) sont trackés.

### Formats d'échange

- **LUCE** : snapshot complet. Magic "LUCE", version, num_shards, fichiers par shard.
  `export_to_snapshot(handle, base_path)` → `Vec<u8>`
- **LUCID** : delta incrémental 1 shard. Magic "LUCID", segments ajoutés/supprimés, meta.
  `export_delta(handle, path, client_segment_ids, client_version)` → `IndexDelta`
- **LUCIDS** : delta multi-shard. Magic "LUCIDS", wraps N LUCID blobs.
  `export_sharded_delta(base_path, client_versions)` → `Vec<u8>`

### Compat layer v2

Le routing se fait dans `build_query()` (lucivy_core/src/query.rs) :
```rust
"term"          → contains + anchor_start + exact_match (si sfx_enabled)
"fuzzy"         → contains + distance (si sfx_enabled)
"regex"         → contains + regex=true (si sfx_enabled)
"phrase"        → contains (si sfx_enabled)
"parse"         → contains pour values simples (si sfx_enabled)
"phrase_prefix" → contains (si sfx_enabled)
"startsWith"    → contains + anchor_start (toujours)
```
Fallback vers le comportement tantivy original si `sfx_enabled=false`.

### QueryConfig — paramètres

```rust
pub struct QueryConfig {
    pub query_type: String,
    pub field: Option<String>,
    pub fields: Option<Vec<String>>,
    pub value: Option<String>,
    pub distance: Option<u8>,          // Levenshtein (0=exact)
    pub anchor_start: Option<bool>,    // SI=0 only
    pub exact_match: Option<bool>,     // match couvre token(s) entier(s)
    pub regex: Option<bool>,           // treat value as regex
    pub strict_separators: Option<bool>,
    pub filters: Option<Vec<FilterClause>>,
    // ... boolean, disjunction_max, more_like_this params
}
```

### WASM — règles critiques

1. **JAMAIS `thread::spawn`** dans les handlers — tout via scheduler
2. `docstore_compress_dedicated_thread: false` en WASM
3. Watch callbacks inline en WASM
4. GC thread skip en WASM
5. `FsWriter` : deferred I/O (tout en RAM jusqu'au `terminate()`)
6. `WRITER_HEAP_SIZE = 15MB` (vs 50MB natif)
7. `MAXIMUM_MEMORY = 4GB` (limit 32-bit WASM)

### luciole — actor system

**Actor lifecycle** :
```
spawn(actor, mailbox) → ActorId
  ↓
scheduler thread picks from ready_queue
  ↓
handle_batch: process up to BATCH_SIZE messages
  ↓
ActorStatus::Continue → re-queue
ActorStatus::Yield → re-queue (give others a chance)
ActorStatus::Suspend → wait for ResumeHandle
ActorStatus::Stop → remove actor
```

**pipe_to pattern** : callback registered BEFORE send → no race condition.
```rust
actor_ref.pipe_to(
    |reply| WorkMsg::Do { reply },
    &target,
    "label",
    |result| TargetMsg::Done(result),
);
```

**collect_replies_to** : N:1 gather with AtomicUsize countdown.
```rust
collect_replies_to(
    receivers,        // Vec<ReplyReceiver<T>>
    &target,          // ActorRef to send result to
    "label",
    |results| TargetMsg::AllDone(results),
);
```

**WaitGraph** : tracks all wait dependencies (Thread/Actor → label).
```rust
let edge_id = wait_graph::register(WaiterKind::Actor(id, name), "drain");
// ... wait ...
wait_graph::unregister(edge_id);
// Auto via WaitGuard (RAII)
```

**ActorActivity** : dynamic labels visible in dumps.
```rust
ctx.set_activity("add_doc 42/500");
// Shows in WARNING dump: ActorId(23) indexer: BUSY add_doc 42/500 (10.0s)
```

## Playground — comment tester

### Lancer le serveur debug

```bash
cd packages/rag3db/extension/lucivy/ld-lucivy/playground
node serve.mjs
# → http://localhost:9877
```

### Build WASM + reload

```bash
cd packages/rag3db/extension/lucivy/ld-lucivy
bash bindings/emscripten/build.sh
# Copie automatiquement dans playground/pkg/

# Reload page (via eval)
curl -s http://localhost:9877/eval/main -d \
  '{"js":"if(window._lucivy)window._lucivy._worker.terminate(); location.reload(true); \"reloading\""}'
```

### Lancer une ingestion

```bash
# Clear logs
echo "" > playground/diag.log

# Attendre que la page soit prête
sleep 5
curl -s http://localhost:9877/eval/main -d \
  '{"js":"document.getElementById(\"status\").textContent"}'
# → "952 documents indexed (lucivy source code)"

# Indexer un repo Git
curl -s http://localhost:9877/eval/main -d \
  '{"js":"document.getElementById(\"gitUrl\").value = \"https://github.com/L-Defraiteur/rag3db\"; cloneGitRepo(); \"started\""}'

# Check status
curl -s http://localhost:9877/eval/main -d \
  '{"js":"document.getElementById(\"status\").textContent"}'
```

### Diagnostics pendant l'ingestion

```bash
# Logs temps réel
tail -f playground/diag.log

# Grep warnings
grep "WARNING" playground/diag.log -A 10

# Grep activity labels
grep "BUSY" playground/diag.log

# Dump WaitGraph (via worker eval — timeout si bloqué)
curl -s http://localhost:9877/eval -d \
  '{"js":"(async()=>await Module.ccall(\"lucivy_dump_wait_graph_text\",\"string\",[],[],{async:true}))()"}'

# Dump scheduler state
curl -s http://localhost:9877/eval -d \
  '{"js":"(async()=>await Module.ccall(\"lucivy_dump_state\",\"string\",[],[],{async:true}))()"}'

# Dump mermaid (threads + actors)
curl -s http://localhost:9877/eval -d \
  '{"js":"(async()=>await Module.ccall(\"lucivy_dump_mermaid\",\"string\",[],[],{async:true}))()"}'
```

### Eval main thread vs worker

```bash
# Main thread (page DOM, navigation)
curl -s http://localhost:9877/eval/main -d '{"js":"document.title"}'

# Worker thread (WASM, Module, index operations)
curl -s http://localhost:9877/eval -d '{"js":"1+1"}'
# Note: timeout si le worker est bloqué (deadlock)
```

### Fonctions C FFI disponibles (via Module.ccall)

| Fonction | Retour | Description |
|----------|--------|-------------|
| `lucivy_dump_mermaid()` | string | Graph mermaid threads + actors |
| `lucivy_dump_state()` | string | Dump texte actors + queue |
| `lucivy_dump_wait_graph()` | string | WaitGraph mermaid |
| `lucivy_dump_wait_graph_text()` | string | WaitGraph texte |
| `lucivy_test_condvar()` | string | Test condvar entre threads |
| `lucivy_test_coop()` | string | Test cooperative wait |
| `lucivy_num_docs(ctx)` | number | Nombre de documents |
| `lucivy_schema_json(ctx)` | string | Schema JSON |

### Standalone (sans serve.mjs)

Le playground fonctionne aussi sans serveur — le `coi-serviceworker.js`
injecte les headers COOP/COEP nécessaires pour SharedArrayBuffer. Les
features debug (eval, POST /log) sont auto-désactivées quand le serveur
n'est pas détecté (probe au démarrage).

## Build commands

```bash
# Tests complets
cargo test --lib                        # 1200 tests
cargo test -p luciole --lib             # 154 tests
cargo test --lib --features mmap,stopwords,lz4-compression,zstd-compression,failpoints --no-default-features  # 1205 tests

# Build WASM
bash bindings/emscripten/build.sh

# Build Python
cd bindings/python && maturin develop --release

# Build Node.js
cargo build -p lucivy-napi --release
cp target/release/liblucivy_napi.so bindings/nodejs/lucivy.node

# Build C++
cargo build -p lucivy-cpp --release
```

## Fichiers clés

| Fichier | Rôle |
|---------|------|
| `lucivy_core/src/query.rs` | Query routing (compat layer, build_query) |
| `lucivy_core/src/sharded_handle.rs` | ShardedHandle (sharding, commit, search) |
| `lucivy_core/src/shard_router.rs` | Token-aware routing |
| `lucivy_core/src/bm25_global.rs` | ExportableStats, merge, cross-shard BM25 |
| `lucivy_core/src/snapshot.rs` | LUCE export/import |
| `lucivy_core/src/sync.rs` | Delta export |
| `lucivy_core/src/directory.rs` | StdFsDirectory, FsWriter (deferred I/O) |
| `lucivy_core/src/handle.rs` | LucivyHandle, WRITER_HEAP_SIZE |
| `src/query/phrase_query/suffix_contains_query.rs` | SuffixContainsQuery (SFX search) |
| `src/query/phrase_query/suffix_contains.rs` | Core SFX walk functions |
| `src/query/phrase_query/regex_continuation_query.rs` | RegexContinuationQuery (fuzzy/regex) |
| `src/query/phrase_query/literal_pipeline.rs` | Multi-token resolution |
| `src/suffix_fst/file.rs` | SFX file reader (prefix_walk, falling_walk) |
| `src/suffix_fst/collector.rs` | SfxCollector (indexing) |
| `src/indexer/indexer_actor.rs` | IndexerActor (docs, flush, drain, finalize) |
| `src/indexer/segment_writer.rs` | SegmentWriter (add_document, activity_reporter) |
| `src/indexer/log_merge_policy.rs` | Merge policy (min_num_segments=8) |
| `src/index/index_meta.rs` | IndexSettings (docstore_compress_dedicated_thread) |
| `luciole/src/scheduler.rs` | Scheduler, ActorContext, ActorActivity |
| `luciole/src/wait_graph.rs` | WaitGraph (deadlock diagnostics) |
| `luciole/src/reply.rs` | Reply, pipe_to, collect_replies_to |
| `luciole/src/generic_actor.rs` | GenericActor (dynamic handlers) |
| `lucistore/src/delta_sharded.rs` | LUCIDS format |
| `bindings/emscripten/src/lib.rs` | WASM binding (all endpoints) |
| `bindings/python/src/lib.rs` | Python binding |
| `bindings/nodejs/src/lib.rs` | Node.js binding |
| `bindings/cpp/src/lib.rs` | C++ binding |
