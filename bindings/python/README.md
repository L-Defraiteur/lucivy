# lucivy 4.0.0

Full-text search for code and technical text, as a library — from Python. Substrings, fuzzy and regex **across token boundaries**, BM25, exact byte spans, every answer checked against the files. Runs in your process, in your transaction (bring your own storage), and the same engine runs in the browser. Powered by Rust, MIT.

[**Try the live playground**](https://l-defraiteur.github.io/lucivy/) — runs entirely in your browser via WASM.

### What's new in 4.0.0

- **The index is 3.7× smaller** — the whole Linux kernel: 18 057 MB in 3.0.8, 4 938 MB in 4.0, 3 344 MB with `derived_in_ram=True`; same answers, same spans, checked against the files ([the comparison with Elasticsearch and tantivy](../../docs/compare-engines-2026-09-05.md))
- **`Index.create(..., shared_dictionary=True)`** — one dictionary of token texts per shard instead of one per segment: 23 % smaller on the kernel, cold queries ×0.8-1.6, same answers; off by default, fixed at creation (also on `create_with_blob_store`)
- **`Index.create(..., dictionary_wait=False)`** — shared dictionary only: a commit returns before the shard's new texts are merged into the dictionary (a background task does it) and, by default, a search waits for that merge so that its cost never depends on when it runs; `False` searches at once over the not-yet-merged parts. Indexing with the dictionary costs ×1.5 (the kernel: 107 s against 56)
- **`Index.create(..., derived_in_ram=True)`** — the three derived sidecars of each segment rebuilt byte for byte when the index opens instead of written: about a third less on disk, the open pays (the kernel: 2 s), never a query; off by default
- **Compatibility contract** — 4.0 opens a 3.0.x index and returns what 3.0.x returned (checked against a fixture the published 3.0.8 wheel built); 3.0.x does not open a 4.0 index; the first commit converts for good

### What 3.0.x brought

- **SFX v3 engine** — per-field suffix FST, the default for every new index; v2 indexes still open
- **Snapshots served from memory** — `Index.open_snapshot(blob)` searches a LUCE snapshot without extracting it
- **Index maintenance** — `compact`, `wait_merges_quiet`, `index_bytes`, `drop_index`
- **Honest queries** — `query_warnings` says what the engine will really search before it runs
- **`parse` query type** — boolean syntax (AND / OR / NOT, quotes, `+`/`-`, parentheses) over substring matching
- **Bring your own storage** — `Index.create_with_blob_store(store, ...)`: index files in any Python object with `load` / `save` / `delete` / `exists` / `list` (a SQLite table, a Postgres `bytea` column, S3...), with lazy loading

### Still there from v2

- **SFX-only engine** — all queries route through the Suffix FST, no legacy code paths
- **Distributed search** — `export_stats` / `merge_stats` / `search_with_global_stats`
- **Incremental sync** — LUCIDS sharded delta export/apply
- **Correct BM25 cross-shard** — identical scores whether 1 shard or 4
- **5 bindings** — Python, Node.js, C++, WASM, Rust

## Install

```bash
pip install lucivy  # 4.0.0 (unpublished yet: 3.0.8 is the last release on PyPI)
```

## Quick start

```python
import lucivy

index = lucivy.Index.create("/tmp/my_index", fields=[
    {"name": "title", "type": "text", "stored": True},
    {"name": "body", "type": "text", "stored": True},
])

index.add(1, title="Rust Programming", body="Systems programming with memory safety")
index.add(2, title="Python Guide", body="Data science and web development")
index.commit()

results = index.search("programming", highlights=True)
for r in results:
    print(r.doc_id, r.score, r.highlights)
```

## API

### Create / open

```python
# Create a new index
index = lucivy.Index.create("/tmp/my_index", fields=[
    {"name": "title", "type": "text", "stored": True},
    {"name": "body",  "type": "text", "stored": True},
    {"name": "score", "type": "f64", "fast": True},
])

# Create a sharded index (4 shards)
index = lucivy.Index.create("/tmp/my_index", fields=[...], shards=4)

# Smaller index: one dictionary per shard instead of one per segment.
# About 20 % less disk and RAM; queries slightly slower at cold cache
# (roughly x1.2 to x1.6 on exact queries, fuzzy ones faster); same answers.
# Fixed at creation.
index = lucivy.Index.create("/tmp/compact", fields=[...], shared_dictionary=True)

# Smaller still on disk: the three derived sidecars of each segment (about
# a third of the index) are rebuilt in RAM, byte for byte, when the index
# is opened, instead of being written. Same answers; opening pays the
# rebuild (never a query), the rebuilt structures stay resident.
index = lucivy.Index.create("/tmp/compact", fields=[...], shared_dictionary=True, derived_in_ram=True)

# Open an existing index
index = lucivy.Index.open("/tmp/my_index")
```

Field types: `"text"` (full-text, tokenized), `"u64"`, `"i64"`, `"f64"`, `"bool"`, `"date"`.

### Add / update / delete

```python
# Fields are passed as keyword arguments
index.add(1, title="Hello", body="World", score=3.14)

index.add_many([
    {"doc_id": 1, "title": "Hello", "body": "World"},
    {"doc_id": 2, "title": "Foo", "body": "Bar"},
])

index.update(1, title="Updated title", body="Updated body")
index.delete(2)
index.commit()
```

### Search

```python
# String query — each word is searched across all text fields (contains_split)
results = index.search("rust async programming")

# Options
results = index.search("rust", limit=20, highlights=True, allowed_ids=[1, 3, 5])

# Retrieve stored field values with results
results = index.search("rust", fields=True)
for r in results:
    print(r.doc_id, r.fields['title'], r.fields['body'])
```

#### contains — substring, fuzzy, regex (cross-token)

All substring queries are cross-token: they match across token boundaries.

```python
# Substring — matches "programming", "programmer", "getProgramHandle", etc.
index.search({"type": "contains", "field": "body", "value": "program"})

# Fuzzy substring (Levenshtein distance)
index.search({"type": "contains", "field": "body", "value": "mutx", "distance": 1})

# Fuzzy with Jaro-Winkler instead of Levenshtein: candidates come from the
# trigram pigeonhole at "distance" (default 2), Jaro-Winkler decides, and
# hits are tiered by similarity (a typo at the end ranks above one at the start)
index.search({"type": "fuzzy", "field": "body", "value": "kmalloc", "fuzzy_metric": "jaro_winkler", "min_similarity": 0.9})

# Regex substring — cross-token regex matching
index.search({"type": "contains", "field": "body", "value": "lock.*mutex", "regex": True})

# Prefix / startsWith — match must start at token boundary (SI=0)
index.search({"type": "startsWith", "field": "body", "value": "prog"})

# Exact whole-token match
index.search({"type": "term", "field": "body", "value": "lock"})

# Phrase — adjacent tokens in order
index.search({"type": "phrase", "field": "body", "value": "mutex lock"})
```

#### contains_split — multi-word search

Split on whitespace, each word becomes a `contains` query, combined with boolean OR.

```python
index.search({"type": "contains_split", "field": "body", "value": "rust safety"})

# With fuzzy distance — each word gets fuzzy tolerance
index.search({"type": "contains_split", "field": "body", "value": "memry safty", "distance": 1})
```

#### parse — search-box syntax

One query type for whatever a user types. A plain value runs as an OR of
substring `contains`, one per word and per field. Boolean syntax — `AND`,
`OR`, `NOT`, `+word`, `-word`, `"quoted phrases"`, parentheses — is lowered
to a `boolean` query of substring `contains` (precedence NOT > AND > OR;
words side by side are OR). Highlights work in both cases.

```python
# Plain words: OR of substring contains, per word × field
index.search({"type": "parse", "value": "web development", "fields": ["title", "body"]})

# Boolean syntax
index.search({"type": "parse", "value": "web AND development", "fields": ["title", "body"]})
index.search({"type": "parse", "value": "rust -deprecated", "fields": ["title", "body"]})
index.search({"type": "parse", "value": '"memory safety" OR (lock AND NOT mutex)', "fields": ["body"]})

# Single field
index.search({"type": "parse", "value": "web", "field": "body"})
```

`query_warnings` tells you which of the two paths a value took.

#### boolean — combine queries with must / should / must_not

```python
index.search({
    "type": "boolean",
    "must": [
        {"type": "contains", "field": "body", "value": "rust"},
    ],
    "should": [
        {"type": "contains", "field": "title", "value": "guide"},
    ],
    "must_not": [
        {"type": "contains", "field": "body", "value": "deprecated"},
    ],
})
```

#### Filtering

Filter on non-text fields (combined with AND):

```python
index.search({
    "type": "contains", "field": "body", "value": "lock",
    "filters": [
        {"field": "category", "op": "eq", "value": "kernel"},
        {"field": "score", "op": "gte", "value": 0.5},
        {"field": "status", "op": "in", "value": ["active", "review"]},
    ]
})
```

Filter ops: `eq`, `ne`, `lt`, `lte`, `gt`, `gte`, `in`, `not_in`, `between`, `starts_with`, `contains`.

Pre-filter by document ID (fast, bitmap-based):

```python
index.search({"type": "contains", "field": "body", "value": "lock"}, allowed_ids=[1, 2, 3])
```

### Query warnings

`query_warnings(query)` returns plain-text warnings about what the engine
will actually search, without running the query: separators ignored in
relaxed mode, a fuzzy distance too loose for the query length, a regex with
no literal to look up (full scan), segments written by the legacy indexer,
which path a `parse` value took. An empty list means nothing applies.

```python
for w in index.query_warnings({"type": "contains", "field": "body", "value": "[0-9]{8}", "regex": True}):
    print("warning:", w)
# warning: "[0-9]{8}" requires no literal the index can look up: every document is scanned whole ...
```

### Snapshots (export / import)

```python
# Export index to a .luce file
index.export_snapshot_to("./backup.luce")

# Export as bytes
blob = index.export_snapshot()

# Import from .luce file
restored = lucivy.Index.import_snapshot_from("./backup.luce", dest_path="./restored_index")

# Import from bytes
with open("./backup.luce", "rb") as f:
    restored = lucivy.Index.import_snapshot(f.read(), dest_path="./restored_index")
```

#### Serve a snapshot without extracting it

`open_snapshot` searches a LUCE snapshot straight from memory: the blob *is*
the index, readers get slices of it, nothing is written to disk. The memory
cost is the blob's own length — `import_snapshot` would hold the blob and the
extracted files at once.

```python
# From bytes
served = lucivy.Index.open_snapshot(blob)

# From a .luce file
served = lucivy.Index.open_snapshot_from("./backup.luce")

served.search("programming")   # same answers as the index it came from
served.index_bytes()           # the live slices of the blob
```

A served snapshot is read-only by construction: `add`, `delete`, `update`,
`commit`, `compact` and the export / delta methods raise `ValueError`, and
`path` is `None`. To edit it, `import_snapshot` it instead.

### Index maintenance

```python
# Merge every shard's segments into segments of at most max_docs documents,
# then commit. Returns the number of merges. Call once after a bulk load;
# not for the browser build.
merges = index.compact(max_docs=10000)

# Block until no background merge is running or about to start — a commit
# returning never meant nothing was merging. Returns the rounds that still
# saw activity. Call before anything that claims a lot of memory (a big
# export_snapshot, a full preload).
index.wait_merges_quiet()

# On-disk bytes of every searchable segment, across all shards
size = index.index_bytes()

# Delete the whole index: commit, release, remove the directory.
# The instance is consumed: every further call raises ValueError.
index.drop_index()
```

### Delta sync (incremental)

Sync only the segments that changed since the client's last version.

```python
# Get current shard versions (property)
versions = index.shard_versions

# Export delta (only changed segments)
delta = index.export_sharded_delta(client_versions)

# Apply delta on the client side
client_index.apply_sharded_delta(delta)
```

### Distributed search

Run BM25 search across multiple machines with correct IDF.

```python
import lucivy

query = {"type": "contains", "field": "body", "value": "mutex"}

# 1. Each node exports its local BM25 stats
stats_a = node_a.export_stats(query)  # JSON string
stats_b = node_b.export_stats(query)  # JSON string

# 2. Coordinator merges stats from all nodes
merged = lucivy.merge_stats([stats_a, stats_b])

# 3. Each node searches with global stats (correct IDF across all nodes)
results_a = node_a.search_with_global_stats(query, merged, limit=10)
results_b = node_b.search_with_global_stats(query, merged, limit=10)

# Restricted to a set of _node_id values, under the same statistics:
# the ids decide which documents are visited, the statistics how they score.
results_a = node_a.search_with_global_stats(query, merged, allowed_ids=[3, 7, 11])

# 4. Coordinator merges top-K results by score
all_results = sorted(results_a + results_b, key=lambda r: r.score, reverse=True)[:10]
```

### Properties

```python
index.num_docs         # number of documents (property, no parentheses)
index.num_shards       # number of shards (property)
index.path             # index directory path (property; None for a served snapshot or a blob store)
index.blob_index_name  # name inside the blob store (property; None otherwise)
index.schema           # list of {"name": "...", "type": "..."} dicts (property)
index.close()          # flush + release writer lock
```

### Bring your own storage (ACID)

An index does not have to live in a directory. Hand it a *blob store* — any
Python object with five methods — and every file the engine writes goes
through it: a SQLite table, a Postgres `bytea` column, S3, anything. The
store is the truth; a local mmap cache is rebuilt from it on every open, so
one database holds the index and any process with a connection can open it.

#### The protocol

```python
class MyBlobStore:
    def load(self, index_name: str, file_name: str) -> bytes: ...
        # Raise FileNotFoundError (or KeyError) when the blob does not exist.
    def save(self, index_name: str, file_name: str, data: bytes) -> None: ...
        # Create or overwrite.
    def delete(self, index_name: str, file_name: str) -> None: ...
        # No error when the blob does not exist.
    def exists(self, index_name: str, file_name: str) -> bool: ...
    def list(self, index_name: str) -> list[str]: ...
        # Every file_name saved under index_name.

    # Optional pair, only for lazy=True (blob_len is required by it):
    def blob_len(self, index_name: str, file_name: str) -> int | None: ...
        # Size in bytes without loading (LENGTH(data) in SQL, HEAD on S3).
    def load_range(self, index_name: str, file_name: str, offset: int, length: int) -> bytes | None: ...
        # A byte range without loading the whole blob (SUBSTR in SQL, a ranged GET).
        # Return None if the backend cannot: the whole blob is loaded instead.
```

`load` and `load_range` may return `bytes`, `bytearray` or a `memoryview`.
`index_name` is a namespace, not the name you passed: shard files are saved
under `"Lucivy_<name>/shard_<i>"`, the root files (`_shard_config.json`,
`_shard_stats.bin`) under `"<name>"` itself. Any other exception raised by
a method is reported by the binding call that needed it (`commit()`,
`search()`, the constructors...) as a `ValueError` carrying its text.

#### Example: SQLite

`save` and `delete` run inside a transaction; a commit that fails halfway
leaves the previous, consistent set of blobs in the table.

```python
import sqlite3, threading

class SqliteBlobStore:
    def __init__(self, path):
        self.lock = threading.Lock()
        # Called from lucivy's threads, hence check_same_thread=False + the lock.
        self.conn = sqlite3.connect(path, check_same_thread=False)
        with self.conn:
            self.conn.execute(
                "CREATE TABLE IF NOT EXISTS blobs ("
                " index_name TEXT NOT NULL, file_name TEXT NOT NULL, data BLOB NOT NULL,"
                " PRIMARY KEY (index_name, file_name))")

    def load(self, index_name, file_name):
        with self.lock:
            row = self.conn.execute(
                "SELECT data FROM blobs WHERE index_name = ? AND file_name = ?",
                (index_name, file_name)).fetchone()
        if row is None:
            raise FileNotFoundError(f"{index_name}/{file_name}")
        return row[0]

    def save(self, index_name, file_name, data):
        with self.lock, self.conn:   # a transaction
            self.conn.execute(
                "INSERT OR REPLACE INTO blobs (index_name, file_name, data) VALUES (?, ?, ?)",
                (index_name, file_name, sqlite3.Binary(data)))

    def delete(self, index_name, file_name):
        with self.lock, self.conn:
            self.conn.execute(
                "DELETE FROM blobs WHERE index_name = ? AND file_name = ?",
                (index_name, file_name))

    def exists(self, index_name, file_name):
        with self.lock:
            return self.conn.execute(
                "SELECT 1 FROM blobs WHERE index_name = ? AND file_name = ?",
                (index_name, file_name)).fetchone() is not None

    def list(self, index_name):
        with self.lock:
            rows = self.conn.execute(
                "SELECT file_name FROM blobs WHERE index_name = ?", (index_name,)).fetchall()
        return [r[0] for r in rows]

    # Optional: lets lazy=True size and probe files without downloading them.
    def blob_len(self, index_name, file_name):
        with self.lock:
            row = self.conn.execute(
                "SELECT length(data) FROM blobs WHERE index_name = ? AND file_name = ?",
                (index_name, file_name)).fetchone()
        return None if row is None else row[0]

    def load_range(self, index_name, file_name, offset, length):
        with self.lock:
            row = self.conn.execute(
                "SELECT substr(data, ?, ?) FROM blobs WHERE index_name = ? AND file_name = ?",
                (offset + 1, length, index_name, file_name)).fetchone()
        return None if row is None else row[0]
```

#### The two constructors

```python
store = SqliteBlobStore("/data/blobs.sqlite")

# Create: same fields / shards as Index.create, plus a name inside the store
index = lucivy.Index.create_with_blob_store(store, "products", fields=[
    {"name": "title", "type": "text", "stored": True},
    {"name": "body",  "type": "text", "stored": True},
], shards=2)
index.add(1, title="Hello", body="World")
index.commit()          # the blobs are in the table now
index.close()

# Open, from any process with the same database — nothing on disk needed
index = lucivy.Index.open_with_blob_store(store, "products")

# Lazy: pull files on first read instead of all at open. Requires blob_len
# on the store (ValueError otherwise); with load_range as well, the small
# probes made while opening a segment do not download anything, and a query
# only pulls what it touches — the suffix FSTs are never downloaded whole.
index = lucivy.Index.open_with_blob_store(store, "products", lazy=True)

# Delete everything through the store (list + delete on every namespace)
index.drop_index()
```

`index.path` is `None` on such an index (`index.blob_index_name` is the
name). Snapshot and delta export read from a directory and raise
`ValueError` here; `close()`, `compact()`, `wait_merges_quiet()`,
`drop_index()` and everything else work as usual.

#### The cache directory

Reads are served from mmap files, so the engine keeps a local copy of what it
uses under `cache_dir` (default: `lucivy-blob-cache` under the system temp
dir). Each open gets a fresh subdirectory, removed when the index is
released. It is disposable: delete it at any time between two opens, the
blobs are the truth.

#### The store runs on lucivy's threads

The store's methods are **not** called from your thread. Commits, merges and
searches run on the engine's own threads, and those threads call the store —
taking the GIL for each call, released again when the call returns. That has
three consequences:

- the store must be **thread-safe**: a lock around a shared connection, or
  one connection per thread (`threading.local()`);
- SQLite specifically needs `check_same_thread=False`;
- a store method must **never call back into the index** (no `search()`,
  no `commit()` from inside `save()`): the index is waiting on that very
  call.

Call `close()` (or `drop_index()`) before the interpreter exits: releasing
an index commits and closes it, which goes through the store one last time.

## License

MIT
