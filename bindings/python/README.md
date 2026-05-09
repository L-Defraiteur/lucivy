# lucivy

Fast BM25 full-text search for Python — with substring matching, fuzzy search, regex, and highlights. Powered by Rust.

## Install

```bash
pip install lucivy
```

## Quick start

```python
import lucivy

index = lucivy.Index.create("./my_index", fields=[
    {"name": "title", "type": "text"},
    {"name": "body", "type": "text"},
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
index = lucivy.Index.create("./my_index", fields=[
    {"name": "title", "type": "text"},
    {"name": "body",  "type": "text"},
    {"name": "tag",   "type": "keyword"},
    {"name": "year",  "type": "u64"},
])

# Create a sharded index (4 shards)
index = lucivy.Index.create("./my_index", fields=[...], shards=4)

# Open an existing index
index = lucivy.Index.open("./my_index")

# Context manager (auto-commit on exit)
with lucivy.Index.open("./my_index") as index:
    index.add(3, title="New doc", body="content")
```

### Add / update / delete

```python
index.add(1, title="Hello", body="World")
index.add_many([
    {"doc_id": 2, "title": "Foo", "body": "Bar"},
    {"doc_id": 3, "title": "Baz", "body": "Qux"},
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

Searches **stored text**, not individual tokens. Handles multi-word phrases, substrings, typos, and regex across token boundaries.

```python
# Substring — matches "programming", "programmer", etc.
index.search({"type": "contains", "field": "body", "value": "program"})

# Multi-word phrase
index.search({"type": "contains", "field": "body", "value": "memory safety"})

# Fuzzy (catches typos)
index.search({"type": "contains", "field": "body", "value": "programing languag", "distance": 1})

# Regex on stored text
index.search({"type": "contains", "field": "body", "value": "program.*language", "regex": True})

# Prefix — match must start at token boundary
index.search({"type": "contains", "field": "body", "value": "prog", "anchor_start": True})

# Exact match — match must cover entire token(s)
index.search({"type": "contains", "field": "body", "value": "rust", "exact_match": True})
```

#### contains_split — one word = one contains query, OR'd together

Like a string query but targeting a specific field.

```python
# "rust safety" -> contains("rust") OR contains("safety") on body
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

#### keyword / range — for non-text fields

```python
index.search({"type": "keyword", "field": "tag", "value": "rust"})
index.search({"type": "range", "field": "year", "gte": 2020, "lte": 2025})
```

### Snapshots (export / import)

```python
# Export index to a .luce file
index.export_snapshot_to("./backup.luce")

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
# On each node: export local stats for this query
query = {"type": "contains", "field": "body", "value": "rust"}
stats_json = node.export_stats(query)

# Coordinator: merge stats from all nodes (manual JSON merge)

# On each node: search with global stats
results = node.search_with_global_stats(query, global_stats_json, limit=10, highlights=True)
```

### Info

```python
index.num_docs()  # number of documents
index.path()      # index directory path
index.schema()    # list of field definitions
index.close()     # flush + release writer lock
```

## License

MIT
