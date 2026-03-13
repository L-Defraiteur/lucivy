# lucivy-core

High-level Rust API for [ld-lucivy](https://crates.io/crates/ld-lucivy) — BM25 full-text search with cross-token fuzzy matching, substring search, regex, and highlights.

This is the recommended way to use Lucivy from Rust. It wraps `ld-lucivy` with schema management, query building, index handles, and snapshot export/import.

Also available as [Python](https://pypi.org/project/lucivy/), [Node.js](https://www.npmjs.com/package/lucivy), and [WASM](https://www.npmjs.com/package/lucivy-wasm) packages.

## Install

```toml
[dependencies]
lucivy-core = "0.1"
```

## Quick start

```rust
use lucivy_core::handle::LucivyHandle;
use lucivy_core::query::{SchemaConfig, FieldDef, QueryConfig};
use lucivy_core::directory::StdFsDirectory;
use std::sync::Arc;

// Define schema
let config = SchemaConfig {
    fields: vec![
        FieldDef { name: "title".into(), field_type: "text".into(), ..Default::default() },
        FieldDef { name: "body".into(), field_type: "text".into(), ..Default::default() },
        FieldDef { name: "tag".into(), field_type: "keyword".into(), ..Default::default() },
        FieldDef { name: "year".into(), field_type: "u64".into(),
                   indexed: Some(true), fast: Some(true), ..Default::default() },
    ],
    tokenizer: None,
    stemmer: None,
};

// Create index
let dir = StdFsDirectory::open("./my_index").unwrap();
let handle = LucivyHandle::create(dir, &config).unwrap();

// Add documents (via the IndexWriter)
let title = handle.field("title").unwrap();
let body = handle.field("body").unwrap();
{
    let mut writer = handle.writer.lock().unwrap();
    let mut doc = ld_lucivy::TantivyDocument::new();
    doc.add_text(title, "Rust Programming");
    doc.add_text(body, "Systems programming with memory safety");
    writer.add_document(doc).unwrap();
    writer.commit().unwrap();
}
handle.reader.reload().unwrap();

// Search
let query = QueryConfig {
    query_type: "contains".into(),
    field: Some("body".into()),
    value: Some("programming".into()),
    ..Default::default()
};
let built = lucivy_core::query::build_query(
    &query, &handle.schema, &handle.index,
    &handle.raw_field_pairs, &handle.ngram_field_pairs, None,
).unwrap();

let searcher = handle.reader.searcher();
let top_docs = searcher.search(
    &built,
    &ld_lucivy::collector::TopDocs::with_limit(10),
).unwrap();
```

## Query types

All queries are built via `QueryConfig` structs, serializable to/from JSON.

### contains — substring, fuzzy, regex (cross-token)

Searches **stored text**, not individual tokens. Handles multi-word phrases, substrings, typos, and regex across token boundaries.

```rust
// Substring — matches "programming", "programmer", etc.
let q = QueryConfig {
    query_type: "contains".into(),
    field: Some("body".into()),
    value: Some("program".into()),
    ..Default::default()
};

// Fuzzy (catches typos, distance=1 by default)
let q = QueryConfig {
    query_type: "contains".into(),
    field: Some("body".into()),
    value: Some("programing languag".into()),
    distance: Some(1),
    ..Default::default()
};

// Regex on stored text (cross-token)
let q = QueryConfig {
    query_type: "contains".into(),
    field: Some("body".into()),
    value: Some("program.*language".into()),
    regex: Some(true),
    ..Default::default()
};
```

### contains_split — one word = one contains, OR'd together

```rust
let q = QueryConfig {
    query_type: "contains_split".into(),
    field: Some("body".into()),
    value: Some("rust safety".into()),
    ..Default::default()
};
```

### startsWith — FST prefix search (faster than contains)

```rust
// Single token: direct prefix match via FST
let q = QueryConfig {
    query_type: "startsWith".into(),
    field: Some("body".into()),
    value: Some("program".into()),
    ..Default::default()
};

// Multi-token: phrase prefix ("programming lang" matches "programming language")
let q = QueryConfig {
    query_type: "startsWith".into(),
    field: Some("body".into()),
    value: Some("programming lang".into()),
    ..Default::default()
};

// Split mode: each word is a separate startsWith, OR'd together
let q = QueryConfig {
    query_type: "startsWith_split".into(),
    field: Some("body".into()),
    value: Some("rust program".into()),
    ..Default::default()
};
```

### boolean — combine queries with must / should / must_not

```rust
let q = QueryConfig {
    query_type: "boolean".into(),
    must: Some(vec![
        QueryConfig {
            query_type: "contains".into(),
            field: Some("body".into()),
            value: Some("rust".into()),
            ..Default::default()
        },
    ]),
    must_not: Some(vec![
        QueryConfig {
            query_type: "contains".into(),
            field: Some("body".into()),
            value: Some("javascript".into()),
            ..Default::default()
        },
    ]),
    ..Default::default()
};
```

### keyword / range — for non-text fields

```rust
// Exact keyword match
let q = QueryConfig {
    query_type: "keyword".into(),
    field: Some("tag".into()),
    value: Some("rust".into()),
    ..Default::default()
};

// Via filters on any query
let q = QueryConfig {
    query_type: "contains".into(),
    field: Some("body".into()),
    value: Some("programming".into()),
    filters: Some(vec![
        lucivy_core::query::FilterClause {
            field: Some("year".into()),
            op: "gte".into(),
            value: Some(serde_json::json!(2023)),
            ..Default::default()
        },
    ]),
    ..Default::default()
};
```

### Highlights

All query types support byte-offset highlights via `HighlightSink`.

```rust
use ld_lucivy::query::HighlightSink;
use std::sync::Arc;

let sink = Arc::new(HighlightSink::new());
let built = lucivy_core::query::build_query(
    &query, &handle.schema, &handle.index,
    &handle.raw_field_pairs, &handle.ngram_field_pairs,
    Some(sink.clone()),
).unwrap();

// After search, read highlights:
let highlights = sink.take(); // HashMap<String, Vec<(u32, u32)>>
```

## Snapshots (export / import)

Portable `.luce` binary format — export an index, import it elsewhere.

```rust
use lucivy_core::snapshot;
use std::path::Path;

// Export to bytes
let data = snapshot::export_index(&handle, Path::new("./my_index")).unwrap();

// Import from bytes
let restored = snapshot::import_index(&data, Path::new("./restored_index")).unwrap();
```

## Directory backends

`LucivyHandle::create(dir, &config)` accepts any `Directory` implementation. Choose the backend that fits your storage:

| Directory | Module | Storage | Use case |
|-----------|--------|---------|----------|
| **StdFsDirectory** | `lucivy_core::directory` | Local filesystem (mmap) | Default for Node.js, Python, C++ bindings |
| **BlobDirectory\<S\>** | `lucivy_core::blob_directory` | Any `BlobStore` (DB, S3, Postgres) + local mmap cache | Durable remote storage — data lives in the store, local cache is ephemeral |
| **MemoryDirectory** | emscripten binding | In-memory (WASM) | Browser / WASM environments |
| **RamDirectory** | `ld_lucivy::directory` | In-memory | Tests |
| **MmapDirectory** | `ld_lucivy::directory` | Local filesystem (mmap) | Low-level, used internally by StdFsDirectory |

### BlobDirectory — "DB stores, mmap serves"

```rust
use lucivy_core::blob_directory::BlobDirectory;
use lucivy_core::blob_store::MemBlobStore; // or CypherBlobStore, S3BlobStore, etc.
use std::sync::Arc;

let store = Arc::new(MemBlobStore::new());
let cache_base = std::env::temp_dir();
let dir = BlobDirectory::new(store, "my_index", &cache_base).unwrap();
let handle = LucivyHandle::create(dir, &config).unwrap();
```

All index files are synced to the `BlobStore` on write, and materialized from it on open. The local cache dir is reference-counted and cleaned up on drop. Index names are auto-prefixed with `Lucivy_` in the store to avoid collisions with other subsystems.

### Implementing a custom BlobStore

```rust
use lucivy_core::blob_store::BlobStore;

impl BlobStore for MyStore {
    fn load(&self, index_name: &str, file_name: &str) -> io::Result<Vec<u8>> { ... }
    fn save(&self, index_name: &str, file_name: &str, data: &[u8]) -> io::Result<()> { ... }
    fn delete(&self, index_name: &str, file_name: &str) -> io::Result<()> { ... }
    fn exists(&self, index_name: &str, file_name: &str) -> io::Result<bool> { ... }
    fn list(&self, index_name: &str) -> io::Result<Vec<String>> { ... }
}
```

## close() — releasing the writer lock

```rust
// Commit pending writes and release the writer lock.
// After close, reads continue but writes return Err("index is closed").
handle.close()?;

// Reopen later (e.g. after process restart)
let dir = StdFsDirectory::open("./my_index")?;
let handle = LucivyHandle::open(dir)?;
```

Necessary when the host process doesn't drop the handle (e.g. rag3db C++ `~Database()` doesn't cascade destruction to extension indexes).

## How contains works

Every text field gets 3 sub-fields automatically:

| Sub-field | Tokenizer | Purpose |
|-----------|-----------|---------|
| `{name}` | stemmed or lowercase | BM25 scoring |
| `{name}._raw` | lowercase only | contains verification (precision) |
| `{name}._ngram` | character trigrams | contains candidate generation |

The `contains` query uses trigram-accelerated substring search:
1. **Candidate collection** via trigram intersection on `._ngram`
2. **Verification** on stored text (fuzzy or regex)
3. **BM25 scoring**

## Lineage

Fork of [tantivy](https://github.com/quickwit-oss/tantivy) v0.26.0 (via [izihawa/tantivy](https://github.com/izihawa/tantivy)).

## License

MIT. See [LICENSE](https://github.com/L-Defraiteur/lucivy/blob/main/LICENSE).
