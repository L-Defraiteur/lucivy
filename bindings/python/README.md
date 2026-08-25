# lucivy v3

Fast BM25 full-text search for Python — with substring matching, fuzzy search, regex, and highlights. Powered by Rust.

[**Try the live playground**](https://l-defraiteur.github.io/lucivy/) — runs entirely in your browser via WASM.

### What's new in v3

- **SFX v3 engine** — per-field suffix FST, the default for every new index; v2 indexes still open
- **Snapshots served from memory** — `Index.open_snapshot(blob)` searches a LUCE snapshot without extracting it
- **Index maintenance** — `compact`, `wait_merges_quiet`, `index_bytes`, `drop_index`
- **Honest queries** — `query_warnings` says what the engine will really search before it runs
- **`parse` query type** — boolean syntax (AND / OR / NOT, quotes, `+`/`-`, parentheses) over substring matching
- **ACID blob storage** — index files stored in a transactional blob store with lazy loading, at the Rust level (see below)

### Still there from v2

- **SFX-only engine** — all queries route through the Suffix FST, no legacy code paths
- **Distributed search** — `export_stats` / `merge_stats` / `search_with_global_stats`
- **Incremental sync** — LUCIDS sharded delta export/apply
- **Correct BM25 cross-shard** — identical scores whether 1 shard or 4
- **5 bindings** — Python, Node.js, C++, WASM, Rust

## Install

```bash
pip install lucivy  # 3.0.0
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

# 4. Coordinator merges top-K results by score
all_results = sorted(results_a + results_b, key=lambda r: r.score, reverse=True)[:10]
```

### Properties

```python
index.num_docs    # number of documents (property, no parentheses)
index.num_shards  # number of shards (property)
index.path        # index directory path (property; None for a served snapshot)
index.schema      # list of {"name": "...", "type": "..."} dicts (property)
index.close()     # flush + release writer lock
```

### ACID blob storage

Index files can live in a transactional blob store instead of a directory:
the `BlobStore` trait, `BlobShardStorage` and lazy loading of segment files
on first read. This is a Rust-level API (`lucivy-core` / `lucistore`), used
by the rag3db extension; it is not exposed in the Python binding yet.

## License

MIT
