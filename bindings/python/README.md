# lucivy v2

Fast BM25 full-text search for Python — with substring matching, fuzzy search, regex, and highlights. Powered by Rust.

[**Try the live playground**](https://l-defraiteur.github.io/lucivy/) — runs entirely in your browser via WASM.

### What's new in v2

- **SFX-only engine** — all queries route through the Suffix FST, no legacy code paths
- **Distributed search** — `export_stats` / `merge_stats` / `search_with_global_stats`
- **Incremental sync** — LUCIDS sharded delta export/apply
- **Correct BM25 cross-shard** — identical scores whether 1 shard or 4
- **5 bindings** — Python, Node.js, C++, WASM, Rust

## Install

```bash
pip install lucivy
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

### Delta sync (incremental)

Sync only the segments that changed since the client's last version.

```python
# Get current shard versions
versions = index.shard_versions()

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
index.path        # index directory path (property)
index.schema      # list of {"name": "...", "type": "..."} dicts (property)
index.close()     # flush + release writer lock
```

## License

MIT
