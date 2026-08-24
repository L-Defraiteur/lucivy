# lucistore

Shared persistence, sync and shard infrastructure for
[lucivy](https://github.com/L-Defraiteur/lucivy) and its friend crates
(`lucivy-core` full-text index, `sparse-vector` sparse index). Depends on
`serde` only.

## What it provides

- **`BlobStore`** — the storage contract: `load`, `save`, `delete`,
  `exists`, `list`, plus optional `blob_len` and `load_range` for backends
  that can answer cheaply (SQL `LENGTH`/`SUBSTRING`, S3 HEAD / ranged GET).
  `MemBlobStore` for tests, `impl BlobStore for Arc<T>` so `Arc<dyn BlobStore>`
  works everywhere.
- **`ShardStorage`** — where an index's shards live: `FsShardStorage`
  (directories) or `BlobShardStorage<S: BlobStore>` (blobs are the source of
  truth, a local cache holds the mmaps).
- **`ShardRouter`** — document-to-shard routing shared by every sharded
  index: round-robin (`balance_weight = 1.0`) or locality-aware (lower
  values co-locate documents sharing tokens), `node_id → shard` map,
  serializable, `resync` from the shards after a crash.
- **Snapshots and deltas** — `snapshot` (LUCE: every shard), `delta` (LUCID:
  one shard) and `delta_sharded` (LUCIDS: only the shards that changed,
  with per-shard versions), all as versioned binary buffers.
- **`sync_server`** — server-side version history per shard: computes the
  delta a client needs, or signals that it is too far behind and needs a
  full snapshot. Engine-agnostic.
- **`blob_cache`** — "the store holds, the mmap serves": blobs materialised
  into a local cache directory, write-through, cleaned up on drop.
- **`version`, `binary`, `fs_utils`** — deterministic version hashes of a
  manifest, length-prefixed binary helpers, filesystem utilities.

## Using it

An index that wants to be persisted anywhere implements its files through a
`BlobStore` and its layout through a `ShardStorage`; the rest (routing,
snapshots, deltas) comes for free and is identical across index types.

```rust
use std::sync::Arc;
use lucistore::blob_store::{BlobStore, MemBlobStore};
use lucistore::shard_router::ShardRouter;

let store: Arc<dyn BlobStore> = Arc::new(MemBlobStore::new());
store.save("my_index", "hello.bin", b"payload")?;
assert_eq!(store.load("my_index", "hello.bin")?, b"payload");

let mut router = ShardRouter::new(4);
let shard = router.route(&[ShardRouter::hash_token("rust"), ShardRouter::hash_token("search")]);
assert!(shard < 4);
# Ok::<(), std::io::Error>(())
```

## License

MIT.
