# sparse-vector

Inverted index for sparse vectors (SPLADE, BM25-as-vectors, learned sparse
embeddings) with WAND pruning. A friend crate of [lucivy](https://github.com/L-Defraiteur/lucivy):
it persists through `lucistore` (filesystem or any `BlobStore`) and shards
behind `luciole` actors, on the same router as the full-text index.

## Features

- **Dense remapping** — token ids are remapped to dense dimensions; each
  dimension owns a posting list whose elements carry a suffix-maximum
  ceiling.
- **WAND search** — windows of record ids are scored at once, ranges that
  cannot reach the current top-k are skipped. Exact results: pruning never
  changes the ranking, only the work.
- **mmap or RAM** — an index is a flat mmap file plus small side files;
  search runs directly over the mapping.
- **Filtered search** — `search_filtered(query, limit, allowed_ids)` seeks
  the allowed ids when they are few, falls back to a set filter otherwise.
- **Sharding** — `ShardedSparseHandle`: N shards, round-robin or
  locality-aware routing (`balance_weight`), routed filtered search (only
  the shards holding allowed ids work), `shard_for_node_id`.
- **Storage backends** — `FsSparseStorage` (directories) or
  `BlobSparseStorage<S: BlobStore>` (blobs are the source of truth, a local
  cache holds the mmaps). Bring your own store: SQL, S3, memory.

## Quick start

```rust
use sparse_vector::handle::SparseHandle;
use sparse_vector::index::SparseVector;

let index = SparseHandle::create("/tmp/sparse_demo")?;
index.insert(1, &SparseVector::new(vec![3, 17, 42], vec![0.8, 0.2, 1.1]))?;
index.insert(2, &SparseVector::new(vec![17, 99], vec![0.5, 0.9]))?;
index.commit_inner()?;

let hits = index.search(&SparseVector::new(vec![17, 42], vec![1.0, 1.0]), 10);
// [(1, 1.3), (2, 0.5)] — (node_id, dot product), best first
# Ok::<(), String>(())
```

Sharded, over a blob store:

```rust
use std::path::Path;
use std::sync::Arc;
use lucistore::blob_store::MemBlobStore;
use sparse_vector::sharded::{ShardedSparseConfig, ShardedSparseHandle};

let store = Arc::new(MemBlobStore::new());
let index = ShardedSparseHandle::create_with_store(
    store, "vectors", Path::new("/tmp/sparse_cache"), &ShardedSparseConfig::new(4))?;
// insert / remove / commit / search / search_filtered / close / drop_index
# Ok::<(), String>(())
```

## Search options

`wand::SearchOptions` controls the loop: `pruning` (on by default),
`window` (ids scored per batch, 4096 by default). `SearchOptions::exhaustive()`
disables pruning — useful as ground truth in tests.

## Design

The design — dimension remapping, ceilings on posting lists, batch scoring of
id windows, WAND-style pruning — is inspired by the sparse index of
[Qdrant](https://github.com/qdrant/qdrant). The code is original; see `NOTICE`.

## License

MIT.
