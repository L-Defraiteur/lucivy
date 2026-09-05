# lucivy-cpp

Full-text search for code and technical text, as a library — from C++. Substrings, fuzzy and regex **across token boundaries**, BM25, exact byte spans, every answer checked against the files. Runs in your process, in your transaction (`lucivy::BlobBackend`), and the same engine runs in the browser. Powered by Rust via a CXX bridge, MIT.

Version 4.0.0 — built on the lucivy 4.0.0 engine (SFX v3 suffix index, shared dictionary optional).

### What's new in 4.0.0

- **The index is 3.7× smaller** — the whole Linux kernel: 18 057 MB in 3.0.8, 4 938 MB in 4.0, 3 344 MB with `"derived_in_ram": true`; same answers, same spans, checked against the files ([the comparison with Elasticsearch and tantivy](../../docs/compare-engines-2026-09-05.md))
- **`lucivy_create` takes a full schema object** — `"shared_dictionary": true` (one dictionary of token texts per shard instead of one per segment: 23 % smaller on the kernel, cold queries ×0.8-1.6, same answers) and `"derived_in_ram": true` (the three derived sidecars rebuilt byte for byte when the index opens: about a third less on disk, the open pays, never a query); both off by default, fixed at creation, also for `lucivy_create_with_backend`
- **`"dictionary_wait": false` in the schema object** — shared dictionary only: a commit returns before the shard's new texts are merged into the dictionary (a background task does it) and, by default, a search waits for that merge so that its cost never depends on when it runs. Indexing with the dictionary costs ×1.5 (the kernel: 107 s against 56)
- **Compatibility contract** — 4.0 opens a 3.0.x index and returns what 3.0.x returned (checked against a fixture the published 3.0.8 wheel built); 3.0.x does not open a 4.0 index; the first commit converts for good

## Build

```bash
cargo build -p lucivy-cpp --release
```

This produces a static library and CXX-generated headers. Link against `liblucivy_cpp.a` and include the generated `lib.rs.h`; add `-I bindings/cpp/include` for the `lucivy/blob_backend.h` header it depends on. The in-memory reference backend (`mem_blob_backend.cc`) is compiled into the library.

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

// Or a full schema object. "shared_dictionary": one dictionary per shard
// instead of one per segment — about 20 % less disk and RAM, queries
// slightly slower at cold cache (roughly x1.2 to x1.6 on exact queries,
// fuzzy ones faster), same answers. Fixed at creation.
auto compact = lucivy::lucivy_create(path,
    R"({"fields":[{"name":"body","type":"text"}],"shards":2,"shared_dictionary":true})", 1);
// "derived_in_ram": the three derived sidecars of each segment (about a
// third of the index) rebuilt in RAM at open instead of written.
auto lean = lucivy::lucivy_create(path,
    R"({"fields":[{"name":"body","type":"text"}],"shared_dictionary":true,"derived_in_ram":true})", 1);

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

## Bring your own storage (ACID)

Besides the filesystem (`lucivy_create` / `lucivy_open`) and in-memory snapshots (`lucivy_open_snapshot`), an index can live in **your** storage: a database table, an object store, anything you can address by `(index_name, file_name)`. Subclass `lucivy::BlobBackend` from `include/lucivy/blob_backend.h` and hand it to the index. The backend is the durable truth; a local cache directory only serves reads through mmap and can be thrown away at any time. This is the standalone equivalent of what rag3db does over Postgres through its own bridge.

Add `-I bindings/cpp/include` to your compiler flags (the generated `lib.rs.h` includes `lucivy/blob_backend.h`).

### The abstract class

```cpp
#include "lucivy/blob_backend.h"

namespace lucivy {
class BlobBackend {
public:
  virtual ~BlobBackend() = default;

  // Fill `out` and return true; return false when the blob does not exist.
  virtual bool load(rust::Str index_name, rust::Str file_name,
                    std::vector<uint8_t>& out) const = 0;
  // Create or overwrite.
  virtual void save(rust::Str index_name, rust::Str file_name,
                    rust::Slice<const uint8_t> data) const = 0;
  // Removing a missing blob is not an error.
  virtual void remove(rust::Str index_name, rust::Str file_name) const = 0;
  virtual bool exists(rust::Str index_name, rust::Str file_name) const = 0;
  // Every file_name under index_name (empty for an unknown index).
  virtual rust::Vec<rust::String> list(rust::Str index_name) const = 0;

  // Optional, for lazy loading. Return false when unknown / unsupported.
  virtual bool blob_len(rust::Str index_name, rust::Str file_name,
                        uint64_t& out) const { return false; }
  virtual bool load_range(rust::Str index_name, rust::Str file_name,
                          uint64_t offset, uint64_t len,
                          std::vector<uint8_t>& out) const { return false; }
};
}
```

`rust::Str` converts to `std::string` with `std::string(s)`; `rust::Slice<const uint8_t>` has `data()` and `size()`.

**Errors and "not found".** Throw any `std::exception` for a failure: the message becomes the error string of the lucivy call that triggered the I/O (`commit()`, `search()`, the open itself). Do not throw for a missing blob: `load` returns `false`, and lucivy treats that as *not found*, a normal answer while opening an index. `blob_len` / `load_range` returning `false` mean "I cannot", and lucivy falls back to loading the whole blob.

**Thread safety.** Every method is `const` and is called **concurrently** from lucivy's scheduler threads: one actor per shard commits in parallel, background merges write while you search. Guard your state with a mutex (make it `mutable`), or use a connection pool, one connection per calling thread.

**No re-entrancy.** A backend must not call back into the index it stores: no `search`, `add` or `commit` from inside `save()`. The calling thread may be the one holding the shard's writer lock.

**Namespaces.** An index created under the name `"products"` stores its per-shard files under `"Lucivy_products/shard_0"`, `"Lucivy_products/shard_1"`, ... and its two root files (`_shard_config.json`, `_shard_stats.bin`) under `"products"`. Treat `index_name` and `file_name` as opaque strings and store both; lock files are never sent to the backend.

### Entry points

```cpp
// Create: config_json is either the fields array of lucivy_create(), or a
// full schema object with the shard count and engine options
// ("shared_dictionary": true for the smaller, per-shard dictionary,
// "derived_in_ram": true for the sidecars rebuilt in RAM instead of written).
auto index = lucivy::lucivy_create_with_blob_store(
    std::make_unique<MyBackend>(connection_string),
    "products",                                           // index_name
    R"({"fields":[{"name":"body","type":"text"}],"shards":2})",
    "",                                                   // cache_dir: "" = temporary
    false);                                               // lazy

// Open what a previous run (or another machine) stored under that name.
auto index = lucivy::lucivy_open_with_blob_store(
    std::make_unique<MyBackend>(connection_string), "products", "/var/cache/lucivy", true);
```

Everything else is the same object: `add`, `commit`, `search`, `compact`, `close`, `drop_index`. What differs:

- **`cache_dir`** holds the mmap cache of the blobs, one subdirectory per shard and per open (`{cache_dir}/{pid}/Lucivy_products/shard_0_{n}/`). It is disposable: the blobs are the truth, a fresh cache is rebuilt from them at the next open. An empty string asks the binding for a temporary directory, removed when the index object is destroyed (call `close()` first, as always). `get_path()` returns the cache directory.
- **`lazy`** — `false`: every blob is loaded at open, predictable latency. `true`: nothing is loaded at open; a file whose size the backend reports (`blob_len`) is pulled on its first *byte read* — the header and footer probes at segment open go through `load_range`, and only the structures a query touches get loaded whole. A backend without `blob_len` / `load_range` still works in lazy mode, it just loads each file when it is first opened.
- **A failed `save()`** makes `commit()` (or `close()`, which commits) return an error and never hangs. The documents of that commit are lost: the segment that could not be persisted is discarded, the committed state is untouched, the index stays usable, and the next `commit()` succeeds with whatever you add afterwards. Re-add what was pending. Segment blobs saved before the failing one may remain in the backend as orphans (never referenced by a `meta.json`). Note that when the failing save is a segment file written by the background finalize, the engine reports a generic `background finalize failed` rather than your exception text; a failing `meta.json` (the commit point) carries it.
- **`drop_index()`** deletes every blob of the index from the backend — the shard namespaces and the root one (`list` + `remove`) — then disarms the instance like a filesystem drop.
- **Not available:** `export_snapshot`, `export_sharded_delta` and `apply_sharded_delta` read shard files from an index directory on disk, which a blob-backed index does not have; they return an explicit error. Your backend already holds the durable copy.

### Reference implementation: in memory

`include/lucivy/mem_blob_backend.h` / `src/mem_blob_backend.cc` is a complete backend over a `std::map` behind a `std::mutex`, shareable between backends through a `std::shared_ptr` (the binding's own tests run on it). Its `load`:

```cpp
bool MemBlobBackend::load(rust::Str index_name, rust::Str file_name,
                          std::vector<uint8_t>& out) const {
  std::lock_guard<std::mutex> lock(map_->mutex);
  auto index = map_->blobs.find(std::string(index_name));
  if (index == map_->blobs.end()) return false;
  auto file = index->second.find(std::string(file_name));
  if (file == index->second.end()) return false;
  out = file->second;
  return true;
}
```

```cpp
#include "lucivy/mem_blob_backend.h"

auto map = lucivy::new_mem_blob_map();
auto index = lucivy::lucivy_create_with_blob_store(
    lucivy::new_mem_blob_backend(map), "demo", fields_json, "", false);
index->add(1, R"({"body":"pthread_mutex_lock"})");
index->close();

// "Another process": a second backend over the same map.
auto again = lucivy::lucivy_open_with_blob_store(
    lucivy::new_mem_blob_backend(map), "demo", "", true);
```

### Postgres sketch

Pseudo-code, not compiled: one table, `SUBSTRING` for ranges.

```sql
CREATE TABLE lucivy_blobs (
  _index TEXT NOT NULL,
  _file  TEXT NOT NULL,
  _data  BYTEA NOT NULL,
  PRIMARY KEY (_index, _file)
);
```

```cpp
class PgBackend : public lucivy::BlobBackend {
  // One connection per calling thread (thread_local, or a pool):
  // lucivy calls these methods concurrently.
  pqxx::connection& conn() const;

public:
  bool load(rust::Str index, rust::Str file, std::vector<uint8_t>& out) const override {
    pqxx::work tx(conn());
    auto rows = tx.exec_params(
        "SELECT _data FROM lucivy_blobs WHERE _index = $1 AND _file = $2",
        std::string(index), std::string(file));
    if (rows.empty()) return false;                  // NotFound, not an error
    auto bytes = rows[0][0].as<pqxx::binarystring>();
    out.assign(bytes.data(), bytes.data() + bytes.size());
    return true;
  }

  void save(rust::Str index, rust::Str file, rust::Slice<const uint8_t> data) const override {
    pqxx::work tx(conn());
    tx.exec_params(
        "INSERT INTO lucivy_blobs (_index, _file, _data) VALUES ($1, $2, $3) "
        "ON CONFLICT (_index, _file) DO UPDATE SET _data = EXCLUDED._data",
        std::string(index), std::string(file),
        pqxx::binarystring(data.data(), data.size()));
    tx.commit();                                     // throws on failure -> commit() error
  }

  void remove(rust::Str index, rust::Str file) const override {
    pqxx::work tx(conn());
    tx.exec_params("DELETE FROM lucivy_blobs WHERE _index = $1 AND _file = $2",
                   std::string(index), std::string(file));
    tx.commit();
  }

  bool exists(rust::Str index, rust::Str file) const override {
    pqxx::work tx(conn());
    return !tx.exec_params("SELECT 1 FROM lucivy_blobs WHERE _index = $1 AND _file = $2",
                           std::string(index), std::string(file)).empty();
  }

  rust::Vec<rust::String> list(rust::Str index) const override {
    pqxx::work tx(conn());
    rust::Vec<rust::String> names;
    for (auto row : tx.exec_params("SELECT _file FROM lucivy_blobs WHERE _index = $1",
                                   std::string(index)))
      names.push_back(row[0].as<std::string>());
    return names;
  }

  // Lazy loading: sizes and ranges without pulling the blob.
  bool blob_len(rust::Str index, rust::Str file, uint64_t& out) const override {
    pqxx::work tx(conn());
    auto rows = tx.exec_params(
        "SELECT LENGTH(_data) FROM lucivy_blobs WHERE _index = $1 AND _file = $2",
        std::string(index), std::string(file));
    if (rows.empty()) return false;
    out = rows[0][0].as<uint64_t>();
    return true;
  }

  bool load_range(rust::Str index, rust::Str file, uint64_t offset, uint64_t len,
                  std::vector<uint8_t>& out) const override {
    pqxx::work tx(conn());
    auto rows = tx.exec_params(                      // SUBSTRING is 1-based
        "SELECT SUBSTRING(_data FROM $3 FOR $4) FROM lucivy_blobs "
        "WHERE _index = $1 AND _file = $2",
        std::string(index), std::string(file), offset + 1, len);
    if (rows.empty()) return false;
    auto bytes = rows[0][0].as<pqxx::binarystring>();
    out.assign(bytes.data(), bytes.data() + bytes.size());
    return true;
  }
};
```

A shard's commit point is its `meta.json`: segment files are saved first, the file registry (`.managed.json`) right before it, then `meta.json` itself. A reader that only trusts files referenced by `meta.json` never sees a half-written commit.

## License

MIT
