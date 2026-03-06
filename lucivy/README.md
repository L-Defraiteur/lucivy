# lucivy (Python)

Python bindings for [ld-lucivy](../README.md) — fast BM25 full-text search with fuzzy substring matching, cross-token regex, and highlights.

## Install

```bash
pip install maturin
maturin develop --release
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

## Search guide

### Use `contains` for everything (cross-token)

The `contains` query operates on **stored text**, not on individual tokens. It handles multi-word phrases, substrings, separators, and typos naturally.

```python
# Substring match
index.search({"type": "contains", "field": "body", "value": "program"})
# Matches "programming", "programmer", etc.

# Multi-word phrase (cross-token)
index.search({"type": "contains", "field": "body", "value": "memory safety"})

# Fuzzy (default distance=1, catches typos)
index.search({"type": "contains", "field": "body", "value": "programing languag", "distance": 1})

# Regex on stored text (cross-token)
index.search({"type": "contains", "field": "body", "value": "program.*language", "regex": True})

# String query = auto contains_split (each word OR'd across all text fields)
index.search("rust async programming")
```

### Do NOT use `type: regex` or `type: fuzzy` for cross-token matching

`regex` and `fuzzy` query types operate on **individual tokens** in the inverted index. They cannot match across token boundaries.

```python
# BAD — this will NOT find "programming language" (two separate tokens)
index.search({"type": "regex", "field": "body", "pattern": "programming language"})

# GOOD — use contains+regex instead
index.search({"type": "contains", "field": "body", "value": "programming language"})
# or with regex pattern:
index.search({"type": "contains", "field": "body", "value": "program.*language", "regex": True})
```

Use `type: fuzzy`/`type: regex` only when you specifically want per-token inverted index behavior.

## Tests

```bash
pytest tests/ -v  # 71 tests
```

See [test_lucivy.py](tests/test_lucivy.py) for comprehensive examples of every query type.
