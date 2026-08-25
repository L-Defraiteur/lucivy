# lucivy-cpp

Fast BM25 full-text search for C++ — with substring matching, fuzzy search, regex, and highlights. Powered by Rust via CXX bridge.

Version 3.0.0 — built on the Lucivy 3.0.0 engine (SFX v3 suffix index).

## Build

```bash
cargo build -p lucivy-cpp --release
```

This produces a static library and CXX-generated headers. Link against `liblucivy_cpp.a` and include the generated `lib.rs.h`.

## Quick start

```cpp
#include "lucivy/src/lib.rs.h"

auto index = lucivy::lucivy_create(
    "/tmp/my_index",
    R"([{"name":"body","type":"text","stored":true}])",
    1  // shards
);

index->add(1, R"({"body":"The pthread_mutex_lock function acquires a mutex"})");
index->add(2, R"({"body":"Use std::lock_guard for RAII mutex management"})");
index->commit();

// Substring search — finds "mutex" inside "pthread_mutex_lock"
auto results = index->search(R"({"type":"contains","field":"body","value":"mutex"})", 10);
for (const auto& r : results) {
    std::cout << "doc=" << r.doc_id << " score=" << r.score << std::endl;
}
```

## API

### Lifecycle

```cpp
// Create a new index
// fields_json: JSON array of field definitions
// Supported types: "text", "u64", "i64", "f64", "bool", "date"
auto index = lucivy::lucivy_create(path, fields_json, shards);

// Open an existing index
auto index = lucivy::lucivy_open(path);

// Commit pending changes
index->commit();

// Flush + release writer lock (index data stays on disk)
index->close();

// Delete the whole index: close it, then remove its directory.
// The instance is disarmed afterwards: every call returns an error
// (num_docs() and index_bytes() answer 0, get_schema() is empty).
index->drop_index();
```

`rollback()` is **not supported** on the sharded handle. It always returns an error and discards nothing: documents added since the last commit stay queued and land at the next `commit()` (or at the next search, which auto-flushes). It only exists so the 2.x header still links.

### Documents

```cpp
// Add a single document (fields as JSON object)
index->add(1, R"({"body":"hello world","score":3.14})");

// Add multiple documents (JSON array, each must have "doc_id")
index->add_many(R"([{"doc_id":2,"body":"foo"},{"doc_id":3,"body":"bar"}])");

// Update (delete + re-add)
index->update(1, R"({"body":"updated content"})");

// Delete
index->remove(1);
index->commit();
```

### Search

All substring queries are cross-token: they match across token boundaries.

```cpp
// Substring
index->search(R"({"type":"contains","field":"body","value":"mutex"})", 10);

// Fuzzy substring (Levenshtein distance)
index->search(R"({"type":"contains","field":"body","value":"mutx","distance":1})", 10);

// Fuzzy substring with Jaro-Winkler similarity instead of an edit distance:
// a candidate matches when its similarity to the value is at least min_similarity
// (0..1); distance then only sizes the candidate window.
index->search(R"({"type":"contains","field":"body","value":"mutx","distance":2,
                 "fuzzy_metric":"jaro_winkler","min_similarity":0.85})", 10);

// Regex substring
index->search(R"({"type":"contains","field":"body","value":"lock.*mutex","regex":true})", 10);

// Prefix / startsWith
index->search(R"({"type":"startsWith","field":"body","value":"pthread"})", 10);

// Phrase — adjacent tokens in order
index->search(R"({"type":"phrase","field":"body","value":"mutex lock"})", 10);

// Multi-word — each word as contains, OR'd together
index->search(R"({"type":"contains_split","field":"body","value":"mutex lock"})", 10);

// Boolean
index->search(R"({"type":"boolean","must":[{"type":"contains","field":"body","value":"lock"}],"must_not":[{"type":"contains","field":"body","value":"clock"}]})", 10);

// With highlights (returns SearchResultWithHighlights with byte offsets)
auto results = index->search_with_highlights(R"({"type":"contains","field":"body","value":"mutex"})", 10);

// Pre-filtered by allowed doc IDs
rust::Vec<uint64_t> ids = {1, 2, 3};
auto results = index->search_filtered(query_json, 10, {ids.data(), ids.size()});
```

A plain JSON string instead of an object (`index->search("\"mutex lock\"", 10)`) runs a `contains_split` over every text field of the schema.

**`parse` — user-typed queries.** Takes a free-form string and one or more fields:

```cpp
// Plain words: OR of substring contains, per word x field
index->search(R"({"type":"parse","fields":["title","body"],"value":"mutex lock"})", 10);

// Boolean syntax: AND / OR / NOT, "quoted phrases", +required / -excluded, parentheses
index->search(R"({"type":"parse","fields":["body"],"value":"mutex AND (lock OR guard) NOT \"spin lock\" -clock"})", 10);
```

A value with none of the operators is an OR of `contains` per word and per field (words side by side are OR'd). A value with boolean syntax becomes a `boolean` query of `contains` clauses, with precedence NOT > AND > OR. Highlights are returned in both cases; `query_warnings()` tells you which path was taken.

**Filtering** on non-text fields:

```cpp
index->search(R"({
    "type":"contains","field":"body","value":"lock",
    "filters":[
        {"field":"category","op":"eq","value":"kernel"},
        {"field":"score","op":"gte","value":0.5}
    ]
})", 10);
```

Filter ops: `eq`, `ne`, `lt`, `lte`, `gt`, `gte`, `in`, `not_in`, `between`, `starts_with`, `contains`.

### Query warnings

Before running a query, ask the engine what it will actually search and where it falls back to a brute-force scan. Returns an empty vector when nothing applies.

```cpp
auto warnings = index->query_warnings(R"({"type":"contains","field":"body","value":"a","distance":2})");
for (const auto& w : warnings) {
    std::cerr << "warning: " << std::string(w) << std::endl;
}
```

### Info

```cpp
index->num_docs();        // total documents across all shards
index->get_path();        // index directory path (empty for a served snapshot)
index->get_schema();      // vector of {name, type}
index->get_schema_json(); // full schema as JSON string
index->index_bytes();     // on-disk bytes of every searchable segment, all shards
```

### Maintenance

```cpp
// Commit, then merge the committed segments of every shard into groups of at
// most max_docs documents (SIZE_MAX: one segment per shard). Blocks until the
// merges are done; returns how many merge rounds reduced a shard's segment count.
size_t merges = index->compact(SIZE_MAX);

// Wait until no background merge is running on any shard. Do this before
// anything that is about to claim memory (a snapshot export, a preload).
// Returns how many rounds saw merge activity.
size_t active = index->wait_merges_quiet();
```

### Snapshots

```cpp
// Export to bytes
auto blob = index->export_snapshot();

// Export to file
index->export_snapshot_to("/backups/my_index.luce");

// Import from bytes (extracts every file into dest, writable)
auto restored = lucivy::lucivy_import_snapshot({blob.data(), blob.size()}, "/tmp/restored");

// Import from file
auto restored = lucivy::lucivy_import_snapshot_from("/backups/my_index.luce", "/tmp/restored");

// Serve a snapshot in place, without extracting it (read-only)
auto served = lucivy::lucivy_open_snapshot({blob.data(), blob.size()});
auto served = lucivy::lucivy_open_snapshot_from("/backups/my_index.luce");
```

`lucivy_open_snapshot` keeps the blob and serves slices of it: nothing is written to disk and the memory cost is the blob's own length. It answers exactly like the index the snapshot came from (same hits, same scores, same highlights). It is **read-only** by construction: `add`/`update`/`remove` are queued but `commit()` fails (and so does `close()`, which commits); `export_snapshot`, delta sync and `drop_index` are refused with an explicit error; `get_path()` is empty. To get a writable copy, use `lucivy_import_snapshot`.

### Delta sync (incremental)

```cpp
// Get current shard versions
auto versions = index->shard_versions();

// Export delta (only changed segments)
auto delta = index->export_sharded_delta(client_versions_json);

// Apply delta on the client side
index->apply_sharded_delta({delta.data(), delta.size()});
```

### Distributed search

```cpp
auto query_json = R"({"type":"contains","field":"body","value":"mutex"})";

// 1. Each node exports its local BM25 stats
auto stats_a = node_a->export_stats(query_json);  // JSON string
auto stats_b = node_b->export_stats(query_json);  // JSON string

// 2. Coordinator merges stats from all nodes
rust::Vec<rust::String> stats_list = {stats_a, stats_b};
auto merged = lucivy::lucivy_merge_stats({stats_list.data(), stats_list.size()});

// 3. Each node searches with global stats (correct IDF)
auto results_a = node_a->search_with_global_stats(query_json, merged, 10);
auto results_b = node_b->search_with_global_stats(query_json, merged, 10);
```

## Storage backends

This binding stores indexes on the filesystem (`lucivy_create` / `lucivy_open`) or serves them from a LUCE blob in memory (`lucivy_open_snapshot`). ACID blob storage — the `BlobStore` trait, `BlobShardStorage`, lazy loading of shards from a database — is a Rust-level API of `lucivy-core` / `lucistore`; rag3db uses it through its own bridge (`lucivy_fts`). It is not part of this standalone binding.

## License

MIT
