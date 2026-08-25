# lucivy-core

The Rust API of [lucivy](https://github.com/L-Defraiteur/lucivy) — BM25
full-text search with **substring matching across token boundaries**, fuzzy
search (Levenshtein or Jaro-Winkler), regex, boolean query syntax and exact
byte-offset highlights. This crate is the recommended entry point: it wraps the
[`ld-lucivy`](https://crates.io/crates/ld-lucivy) engine with a sharded handle,
a JSON query builder, snapshots and deltas, and pluggable storage.

Also available as [Python](https://pypi.org/project/lucivy/),
[Node.js](https://www.npmjs.com/package/lucivy),
[browser (WASM)](https://www.npmjs.com/package/lucivy-wasm) and C++ packages,
all on this crate. Everything is MIT.

```toml
[dependencies]
lucivy-core = "3.0"
```

## Quick start

```rust
use lucivy_core::sharded_handle::ShardedHandle;
use lucivy_core::query::{QueryConfig, SchemaConfig};

// A schema is JSON — the same shape every binding takes.
let config: SchemaConfig = serde_json::from_value(serde_json::json!({
    "fields": [
        { "name": "path",    "type": "text" },
        { "name": "content", "type": "text" },
        { "name": "stars",   "type": "u64", "fast": true }
    ],
    "shards": 4
}))?;

let index = ShardedHandle::create("/tmp/my_index", &config)?;
index.add_document_json(1, &serde_json::json!({
    "path": "src/lock.c",
    "content": "void *buf = kmalloc(size, GFP_KERNEL); spin_lock_init(&lock);",
    "stars": 3
}))?;
index.commit()?;

// Substring, across token boundaries, with byte spans per field.
let query = QueryConfig {
    query_type: "contains".into(),
    field: Some("content".into()),
    value: Some("lock_init".into()),
    ..Default::default()
};
for hit in index.search_with_docs(&query, 10)? {
    println!("{:.3} shard {} spans {:?}", hit.score, hit.shard_id, hit.highlights);
    // hit.doc is the stored document (ld_lucivy::LucivyDocument)
}
index.close()?;
# Ok::<(), String>(())
```

## Queries

One `QueryConfig` (also deserialised from JSON) covers every query type:

| `query_type` | what runs |
|---|---|
| `contains` | substring across tokens — `distance` makes it fuzzy, `regex: true` a regex, `anchor_start` / `exact_match` bound it to words |
| `contains_split` | each whitespace-separated word is a `contains`, OR'd |
| `startsWith`, `term`, `phrase` | at a word start · covering whole words · adjacent words in order |
| `fuzzy`, `regex` | aliases of `contains` + `distance` / `+ regex` |
| `parse` | plain value: OR of `contains` per word × field; boolean syntax `AND` / `OR` / `NOT`, quotes, `+` / `-`, parentheses |
| `boolean`, `disjunction_max` | composition (`must` / `should` / `must_not`) |
| `more_like_this` | TF-IDF similarity |

```rust
# use lucivy_core::query::QueryConfig;
// Fuzzy by Jaro-Winkler: the trigram pigeonhole at `distance` (default 2)
// generates candidates, the similarity decides and ranks.
let jw = QueryConfig {
    query_type: "fuzzy".into(),
    field: Some("content".into()),
    value: Some("kmaloc".into()),
    fuzzy_metric: Some("jaro_winkler".into()),
    min_similarity: Some(0.9),
    ..Default::default()
};

// Boolean syntax over several fields.
let parsed = QueryConfig {
    query_type: "parse".into(),
    fields: Some(vec!["content".into(), "path".into()]),
    value: Some("kmalloc AND NOT kfree".into()),
    ..Default::default()
};

// Strict separators: `spin_lock` must not match `spin-lock`.
let strict = QueryConfig {
    query_type: "contains".into(),
    field: Some("content".into()),
    value: Some("spin_lock".into()),
    strict_separators: Some(true),
    ..Default::default()
};
```

Separators are *relaxed* by default: `_`, `-`, `.`, `/` and spaces are ignored
on both sides, so `rag3weaver` finds `rag3_weaver`; the highlight covers the
real bytes. Non-text fields are filtered with `filters` (`eq`, `ne`, `lt`,
`lte`, `gt`, `gte`, `in`, `not_in`, `between`, `starts_with`, `contains`).

`index.query_warnings(&query)` returns, without running anything, what the
engine will really do — separators ignored, a distance that rewrites most of a
short query, a regex with no usable literal that has to scan.

`index.search_filtered(&query, limit, None, allowed_ids)` (a `HashSet<u64>`)
pre-filters by document id and only wakes the shards that hold those ids.

## Sharding and distribution

`ShardedHandle` runs N shards in parallel over a shared actor pool
([`luciole`](https://crates.io/crates/luciole)); BM25 statistics are aggregated
over every shard before scoring, so scores are **identical** with 1 or 4 shards.
Routing is `balance_weight = 1.0` (round-robin, fastest indexing) or lower
(token-aware, co-locates similar documents).

For several machines: `export_stats(&query)` on each node,
`ExportableStats::merge(&stats)` on the coordinator,
`search_with_global_stats(&query, &merged, limit)` on each node, then merge the
top-k — every node scores with the global IDF.

## Snapshots and deltas

```rust
# use lucivy_core::sharded_handle::ShardedHandle;
# use lucivy_core::snapshot;
# fn demo(index: &ShardedHandle) -> Result<(), String> {
// LUCE: every shard's live files in one blob.
let blob: Vec<u8> = snapshot::export_to_snapshot(index, std::path::Path::new("/tmp/my_index"))?;

// Extract it into a fresh directory…
let copy = snapshot::import_from_snapshot(&blob, std::path::Path::new("/tmp/copy"))?;

// …or serve it in place, read-only, without extracting: the blob *is* the
// index, its files are slices of it. Memory cost = the blob's length.
let served = ShardedHandle::open_snapshot(ld_lucivy::directory::OwnedBytes::new(blob))?;
assert!(served.is_read_only());
# Ok(()) }
```

Incremental sync: `export_sharded_delta(&client_versions)` on the server ships
only the shards that changed (LUCIDS, with their `.del` files);
`apply_sharded_delta(&delta)` on the client writes the new segments and reloads.

## Bring your own storage (ACID)

An index's files are blobs. Implement `lucistore::BlobStore` — `load`, `save`,
`delete`, `exists`, `list`, plus the optional `blob_len` / `load_range` pair —
and lucivy runs on it: the blobs are the truth, the local mmap cache is
disposable. A transactional database becomes the store; rag3db does this over
Postgres.

```rust
# use std::sync::Arc;
# use lucivy_core::sharded_handle::ShardedHandle;
# use lucivy_core::query::SchemaConfig;
use lucivy_core::blob_directory::{BlobShardStorage, BlobLoadMode};
use lucistore::blob_store::MemBlobStore;   // your own BlobStore in real life

# fn demo(config: &SchemaConfig) -> Result<(), String> {
let store = Arc::new(MemBlobStore::new());
let cache = std::env::temp_dir().join("lucivy-cache");
let storage = BlobShardStorage::new(store.clone(), "my_index", &cache);
let index = ShardedHandle::create_with_storage(Box::new(storage), config)?;
// …
index.close()?;

// Reopen lazily: a file is pulled on its first byte read (blob_len / load_range).
let storage = BlobShardStorage::new(store, "my_index", &cache).with_load_mode(BlobLoadMode::Lazy);
let index = ShardedHandle::open_with_storage(Box::new(storage))?;
# let _ = index; Ok(()) }
```

The store's methods run on lucivy's scheduler threads: they must be thread-safe
and must not call back into the index.

## Maintenance and memory

- `compact(max_docs)` — merge every shard down to segments of at most `max_docs`
  documents, then commit; once after a bulk load.
- `wait_merges_quiet()` — a commit returning never meant "nothing is merging";
  call this before measuring, exporting or preloading.
- `index_bytes()`, `residency()`, `memory_warnings()`, `preload()` — how big
  the index is, whether this build holds it in memory or streams it
  (`LUCIVY_RAM_INDEX_MAX`, 3 GB on wasm32), and loading it once.
- `drop_index()` — close and delete every file, through the store if any.

Environment knobs (`LUCIVY_SFX_HEAP`, `LUCIVY_MAX_PENDING_FINALIZE`,
`LUCIVY_MAX_INFLIGHT_DOCS`, `LUCIVY_MAX_MERGED_DOCS`, `LUCIVY_MERGE_CONCURRENCY`,
`LUCIVY_SCHEDULER_THREADS`, `LUCIVY_VERBOSE`) bound indexing memory and
parallelism; the defaults are the measured ones on native and on wasm32.
`LUCIVY_HIGHLIGHT_SPAN_CAP` (4 M native, 1 M wasm) bounds the spans a search
records before it repeats itself for its top-k only, and
`LUCIVY_MAX_MATCHES_PER_SEGMENT` (4 M native, 20 k wasm) bounds what one
segment resolves for one query — a one-letter query over a large corpus
otherwise produces tens of millions of both; past the cap it is truncated on
that segment rather than killing the process.

## How it works

Every suffix of every token goes into a Suffix FST, partitioned by where it
starts (token start, inside a token, whole word); a sibling table records which
token follows which, so a walk can cross token boundaries; every match is then
verified on the source text — Levenshtein or Jaro-Winkler for fuzzy, the regex
itself for regex — which is what makes the spans exact. The design, the segment
files and the measurements are in the repository's
[ARCHITECTURE.md](https://github.com/L-Defraiteur/lucivy/blob/main/ARCHITECTURE.md).

## License

MIT.
