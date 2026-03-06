# lucivy

Fork of [tantivy](https://github.com/quickwit-oss/tantivy) v0.26.0 — a fast full-text search engine in Rust — with **fuzzy substring search**, **contains_split multi-word matching**, **trigram-accelerated regex**, and **BM25 scoring**. Designed for code search, technical docs, and mixed-language content where exact matching fails.

```
quickwit-oss/tantivy v0.22
  -> izihawa/tantivy v0.26.0
    -> L-Defraiteur/lucivy (this fork)
```

## Python package

lucivy ships with Python bindings via [PyO3](https://pyo3.rs) + [maturin](https://www.maturin.rs/). Use it as a standalone BM25 index alongside vector databases like Qdrant, Milvus, etc.

```bash
cd lucivy && pip install maturin && maturin develop --release
pytest tests/  # 71 tests
```

```python
import lucivy

# Create an index with text + filter fields
index = lucivy.Index.create("./my_index", fields=[
    {"name": "title", "type": "text"},
    {"name": "body", "type": "text"},
    {"name": "category", "type": "string"},
    {"name": "year", "type": "i64", "indexed": True, "fast": True},
], stemmer="english")

# Add documents
index.add(1, title="Rust programming guide", body="Learn systems programming with Rust", year=2024)
index.add(2, title="Python for data science", body="Data analysis with pandas and numpy", year=2023)
index.add(3, title="C++ template metaprogramming", body="Advanced C++ techniques", year=2022)
index.commit()

# Search — string queries use contains_split: each word is a fuzzy
# substring match, combined with boolean OR, across all text fields
results = index.search("rust program", limit=10)

# With highlights — get byte offsets of matches per field
results = index.search("rust", limit=10, highlights=True)
for r in results:
    print(r.doc_id, r.score, r.highlights)
    # highlights = {"title": [(0, 4)], "body": [(42, 46)]}

# Pre-filtered by document IDs (for hybrid search with vector DBs)
results = index.search("programming", limit=10, allowed_ids=[1, 3])

# Delete, update, persistence
index.delete(2)
index.update(1, title="Updated title", body="Updated body", year=2025)
index.commit()  # required to persist changes

# Reopen from disk
index2 = lucivy.Index.open("./my_index")
```

### Python API

| Method | Description |
|--------|-------------|
| `Index.create(path, fields, stemmer?)` | Create a new index |
| `Index.open(path)` | Open an existing index |
| `index.add(doc_id, **fields)` | Add a document |
| `index.add_many([{doc_id, ...}])` | Batch add |
| `index.delete(doc_id)` | Delete a document |
| `index.update(doc_id, **fields)` | Delete + re-add |
| `index.commit()` / `index.rollback()` | Flush / discard |
| `index.search(query, limit, highlights?, allowed_ids?)` | Search |
| `index.num_docs` / `index.schema` / `index.path` | Properties |

### Query types

lucivy has two categories of queries — **cross-token** (operates on stored text) and **per-token** (operates on inverted index terms). For most use cases, use `contains`.

#### Cross-token queries (recommended)

These match against the **stored text** of a field. They handle multi-word phrases, substrings, separators, and special characters naturally.

**`contains`** — the workhorse query. Fuzzy substring match with separator awareness.

```python
# Exact substring
index.search({"type": "contains", "field": "body", "value": "programming language"})

# Substring within a token: "program" matches "programming"
index.search({"type": "contains", "field": "body", "value": "program"})

# Fuzzy tolerance (default distance=1, catches typos)
index.search({"type": "contains", "field": "body", "value": "programing languag", "distance": 1})

# Strict exact: distance=0 disables fuzzy
index.search({"type": "contains", "field": "body", "value": "programming", "distance": 0})
```

**`contains` + `regex: true`** — regex on stored text (cross-token).

```python
# Matches "programming language" — the .* spans the space between tokens
index.search({"type": "contains", "field": "body", "value": "program.*language", "regex": True})

# Alternation
index.search({"type": "contains", "field": "body", "value": "python|rust", "regex": True})
```

**`contains_split`** — splits query into words, each word is a `contains`, combined with OR.

```python
# String query (auto contains_split across all text fields)
index.search("rust async programming")

# Explicit dict query on a specific field
index.search({"type": "contains_split", "field": "body", "value": "memory safety"})

# With fuzzy tolerance per word
index.search({"type": "contains_split", "field": "body", "value": "memry safty", "distance": 1})
```

**`boolean`** — combine sub-queries with must (AND), should (OR), must_not (NOT).

```python
index.search({
    "type": "boolean",
    "must": [{"type": "contains", "field": "body", "value": "web"}],
    "must_not": [{"type": "contains", "field": "title", "value": "javascript"}],
})
```

#### Per-token queries (advanced)

These operate on **individual tokens** in the inverted index. They cannot match across token boundaries.

| Type | Use case | Example |
|------|----------|---------|
| `fuzzy` | Single-word Levenshtein | `{"type": "fuzzy", "field": "title", "value": "pythn", "distance": 1}` |
| `regex` | Regex on single tokens | `{"type": "regex", "field": "body", "pattern": "program.*"}` |
| `term` | Exact token lookup | `{"type": "term", "field": "title", "value": "rust"}` |
| `phrase` | Exact phrase (stemmed) | `{"type": "phrase", "field": "body", "terms": ["rust", "programming"]}` |
| `parse` | Query parser syntax | `{"type": "parse", "field": "body", "value": "rust AND programming"}` |

> **When to use per-token vs cross-token?**
> Use `contains` (cross-token) for almost everything. Use per-token queries only when you specifically need inverted index behavior (e.g. exact token lookup, or query parser syntax).

#### Filters on non-text fields

Non-text fields (`i64`, `f64`, `u64`, `string`) can be filtered via the `filters` key. Fields must be created with `"indexed": True, "fast": True`.

```python
index.search({
    "type": "contains",
    "field": "body",
    "value": "programming",
    "filters": [
        {"field": "year", "op": "gte", "value": 2023},
    ],
})
# Supported ops: eq, ne, lt, lte, gt, gte, in, not_in, between, starts_with, contains
```

#### Highlights

All query types support byte-offset highlights. Internal fields (`._raw`, `._ngram`) are automatically filtered out.

```python
results = index.search("rust programming", highlights=True)
for r in results:
    if r.highlights:
        for field, offsets in r.highlights.items():
            print(f"  {field}: {offsets}")  # e.g. "body": [(5, 9), (20, 31)]
```

## Internals

### Triple-field layout

Every text field automatically gets 3 sub-fields:

| Sub-field | Tokenizer | Used by |
|-----------|-----------|---------|
| `{name}` | stemmed or lowercase | `phrase`, `parse` queries (recall) |
| `{name}._raw` | lowercase only | `term`, `fuzzy`, `regex`, `contains` (precision) |
| `{name}._ngram` | character trigrams | `contains` candidate generation |

This is transparent to the user — you always reference the base field name.

### NgramContainsQuery — how `contains` works

The `contains` query type uses trigram-accelerated substring search on stored text:

1. **Candidate collection** — depends on mode:
   - *Fuzzy*: exact lookup on `._raw` + trigram intersection on `._ngram`
   - *Regex*: trigram union on `._ngram` from extracted regex literals
   - *Short literals*: full segment scan when literals < 3 chars
2. **Verification** — read stored text, dispatch to fuzzy or regex verifier
3. **BM25 scoring** — standard `idf * (1 + k1) * tf / (tf + k1 * (1 - b + b * dl / avgdl))`

Three verification modes:

- **Fuzzy** (default) — token-by-token Levenshtein with separator validation
- **Regex** (`regex: true`) — compiled regex on stored text
- **Hybrid** — regex OR fuzzy, returns `max(tf_regex, tf_fuzzy)`

### What `contains` matches

**Fuzzy mode** (default):

| Query | Document | Match? | Why |
|-------|----------|--------|-----|
| `programming` | `"Rust programming is fun"` | yes | exact token match |
| `programing` (typo) | `"Rust programming is fun"` | yes | fuzzy distance=1 |
| `program` | `"Rust programming is fun"` | yes | substring of token |
| `programming language` | `"...programming language used..."` | yes | cross-token with separator |
| `c++` | `"c++ and c# are popular"` | yes | separator-aware |
| `std::collections` | `"use std::collections::HashMap"` | yes | multi-token + `::` separator |

**Regex mode** (`regex: true`):

| Pattern | Document | Match? | Why |
|---------|----------|--------|-----|
| `program.*language` | `"...programming language used..."` | yes | cross-token regex on stored text |
| `python\|rust` | `"Python is versatile"` | yes | alternation |
| `v[0-9]+` | `"version v2.0 released"` | yes | full-scan fallback (literal < 3 chars) |

### FST-based fallback

When no `._ngram` field is available, `contains` falls back to a 4-level cascade on the term dictionary:

1. **Exact** — term dict lookup
2. **Fuzzy** — Levenshtein automaton
3. **Substring** — regex `.*token.*`
4. **Fuzzy substring** — combined Levenshtein + substring automaton

### HighlightSink

Thread-safe side-channel for capturing match byte offsets during scoring. Zero extra cost — offsets are captured as a byproduct of existing verification.

Supported by: `contains`, `term`, `fuzzy`, `regex`, `phrase`, and all boolean compositions.

### WithFreqsAndPositionsAndOffsets

New `IndexRecordOption` variant that stores `(offset_from, offset_to)` per token in the postings, like Lucene's `DOCS_AND_FREQS_AND_POSITIONS_AND_OFFSETS`.

## Building

```bash
# Rust library tests (1064 tests)
cargo test --lib

# Python bindings + tests
cd lucivy
maturin develop --release
pytest tests/ -v  # 71 tests
```

## Usage as a Rust dependency

```toml
[dependencies]
ld-lucivy = { path = "../ld-lucivy", features = ["stopwords", "lz4-compression", "stemmer"] }
```

Also used by [lucivy_fts](../../lucivy_fts/), a cxx FFI crate that exposes full-text search for [rag3db](https://github.com/L-Defraiteur/rag3db).

## Lineage

- [quickwit-oss/tantivy](https://github.com/quickwit-oss/tantivy) — original full-text search engine in Rust
- [izihawa/tantivy](https://github.com/izihawa/tantivy) — v0.26.0 fork with regex phrase queries, FST improvements
- **L-Defraiteur/lucivy** — this fork: NgramContainsQuery, contains_split, fuzzy + regex + hybrid modes, BM25, HighlightSink, byte offsets, Python bindings

## License

LRSL (Luciform Research Source License) v1.2 — source-available. Free for research, personal, academic use and businesses under 100k EUR annual revenue. Commercial use above threshold requires a separate agreement. See [LICENSE](LICENSE) for details.

The original Tantivy code is MIT-licensed (see [NOTICE](NOTICE)).
