# lucivy-cpp

Fast BM25 full-text search for C++ — with substring matching, fuzzy search, regex, and highlights. Powered by Rust via CXX bridge.

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
```

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

### Info

```cpp
index->num_docs();        // total documents across all shards
index->get_path();        // index directory path
index->get_schema();      // vector of {name, type}
index->get_schema_json(); // full schema as JSON string
```

### Snapshots

```cpp
// Export to bytes
auto blob = index->export_snapshot();

// Export to file
index->export_snapshot_to("/backups/my_index.luce");

// Import from bytes
auto restored = lucivy::lucivy_import_snapshot(blob.data(), blob.size(), "/tmp/restored");

// Import from file
auto restored = lucivy::lucivy_import_snapshot_from("/backups/my_index.luce", "/tmp/restored");
```

### Delta sync (incremental)

```cpp
// Get current shard versions
auto versions = index->shard_versions();

// Export delta (only changed segments)
auto delta = index->export_sharded_delta(client_versions_json);

// Apply delta on the client side
index->apply_sharded_delta(delta.data(), delta.size());
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

## License

MIT
