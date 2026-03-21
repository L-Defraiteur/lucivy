# Distributed ACID Search — Postgres Example

Full working example of lucivy with ACID persistence (Postgres-backed BlobStore)
and distributed search across multiple nodes with unified BM25 + highlights.

## Quick Start

```bash
# 1. Start Postgres
docker compose -f docker-compose.test.yml up -d

# 2. Run the example tests
POSTGRES_URL="host=localhost port=5433 user=test password=test dbname=lucivy_test" \
  cargo test --package lucivy-core --test acid_postgres -- --ignored --nocapture

# 3. Cleanup
docker compose -f docker-compose.test.yml down -v
```

## What's Demonstrated

### ACID Persistence

Every segment file (postings, store, suffix FST, meta.json...) is stored as
a blob in Postgres. The local filesystem is just a mmap cache for zero-copy reads.

```
Write: doc → segment → BlobStore (Postgres) + local cache (mmap)
Read:  local cache → mmap zero-copy (fast)
Crash: cache lost → rebuilt from Postgres on reopen (all data survives)
```

**Lock files are NOT persisted** — they're process-local (OS flock).
This means reopen after crash always works, no manual cleanup needed.

### Distributed Indexation

Each node indexes independently. No coordination needed at index time.

```
             ┌─ Node A: ShardedHandle("products_a") ─ 2 shards ─ Postgres
Client ──────┤
             └─ Node B: ShardedHandle("products_b") ─ 2 shards ─ Postgres
```

The client decides which node gets which document. Options:

- **By ID range**: docs 0-49 → node A, docs 50-99 → node B
- **By hash**: `hash(doc_id) % num_nodes` → consistent routing
- **By category**: products → node A, articles → node B
- **Round-robin**: simple load distribution

Each node has its own `ShardedHandle` with its own internal sharding
(token-aware routing across its local shards). The nodes don't talk
to each other during indexation.

```rust
// Node A (could be a separate process/machine):
let store = Arc::new(PostgresBlobStore::connect("postgres://db_a/...")?);
let storage = BlobShardStorage::new(store, "products", &cache_path);
let node_a = ShardedHandle::create_with_storage(Box::new(storage), &config)?;

for doc in my_batch {
    node_a.add_document(doc, node_id)?;
}
node_a.commit()?;
```

### Distributed Search (3 phases)

Search requires coordination to compute globally consistent BM25 scores.
Without this, IDF (inverse document frequency) would be computed per-node,
making scores incomparable across nodes.

#### Phase 1 — Collect Stats (~60 bytes per node)

```
Coordinator ──→ Node A: "what are your stats for query 'lock'?"
            ──→ Node B: "what are your stats for query 'lock'?"

Node A ──→ { total_docs: 50, term_freqs: {"lock": 35} }   // 58 bytes JSON
Node B ──→ { total_docs: 50, term_freqs: {"lock": 42} }   // 58 bytes JSON
```

```rust
// Each node (HTTP endpoint: POST /stats):
let stats = handle.export_stats(&query_config)?;
let json = serde_json::to_string(&stats)?;
```

#### Phase 2 — Merge Stats (coordinator, no data access)

```rust
let global = ExportableStats::merge(&[stats_a, stats_b]);
// global.total_num_docs = 100
// global.doc_freqs["lock"] = 77
```

The coordinator sends `global` back to each node.

#### Phase 3 — Search + Highlights (parallel, each node)

```
Coordinator ──→ Node A: "search 'lock' with these global stats, top 10"
            ──→ Node B: "search 'lock' with these global stats, top 10"

Node A ──→ [{score: 0.057, doc: {...}, highlights: {"body": [[47,51]]}}]
Node B ──→ [{score: 0.055, doc: {...}, highlights: {"body": [[32,36]]}}]
```

```rust
// Each node (HTTP endpoint: POST /search):
let results = handle.search_with_global_stats(
    &query_config, top_k, &global_stats, Some(highlight_sink),
)?;
```

The coordinator merges by score (binary heap) and returns top-K globally.

**Highlights are resolved on each node** — no extra round-trip.

### Convenience: `search_with_docs()`

For local (non-distributed) use, `search_with_docs()` returns results with
documents and highlights already resolved:

```rust
let hits = handle.search_with_docs(&query, 10)?;
for hit in &hits {
    println!("score={:.4} highlights={:?}", hit.score, hit.highlights);
    // hit.doc is the full LucivyDocument
}
```

## Network Adaptation

The example runs in one process. For real distributed deployments,
each node exposes 3 HTTP endpoints:

```
POST /index    →  node.add_document(doc)         (fire-and-forget)
POST /commit   →  node.commit()                  (sync)
POST /stats    →  node.export_stats(query)        →  ExportableStats JSON (~60 bytes)
POST /search   →  node.search_with_global_stats() →  Vec<SearchHit> JSON
```

The coordinator is stateless — it just orchestrates the 3-phase protocol.

## BlobStore Backends

Each node can use any storage backend. Implement the `BlobStore` trait
(5 methods: `load`, `save`, `delete`, `exists`, `list`) — ~50 lines per backend.

```rust
// Postgres (this example)
let store = PostgresBlobStore::connect("postgres://...")?;

// S3 (implement S3BlobStore)
let store = S3BlobStore::new("s3://bucket/indexes")?;

// Mixed: node A in Postgres, node B on S3
// Each node creates its own BlobShardStorage independently
```

Nodes don't need to use the same backend. Node A can be Postgres,
node B can be S3, node C can be local filesystem. The search protocol
only exchanges stats and results — never raw index data.

## Files

- `acid_postgres.rs` — Full working example (4 tests)
- `../../docker-compose.test.yml` — Postgres 16 for testing
- `../../lucivy_core/src/bm25_global.rs` — `ExportableStats` (serializable BM25 stats)
- `../../lucivy_core/src/sharded_handle.rs` — `search_with_global_stats()`, `search_with_docs()`
- `../../lucivy_core/src/blob_directory.rs` — `BlobDirectory` (ACID mmap + BlobStore)
