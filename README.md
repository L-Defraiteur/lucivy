# lucivy 3.0.3

[![PyPI](https://img.shields.io/pypi/v/lucivy?label=PyPI&color=blue)](https://pypi.org/project/lucivy/)
[![npm](https://img.shields.io/npm/v/lucivy?label=npm&color=cb3837)](https://www.npmjs.com/package/lucivy)
[![npm wasm](https://img.shields.io/npm/v/lucivy-wasm?label=npm%20wasm&color=cb3837)](https://www.npmjs.com/package/lucivy-wasm)
[![crates.io](https://img.shields.io/crates/v/lucivy-core?label=crates.io&color=e6522c)](https://crates.io/crates/lucivy-core)
[![CI](https://github.com/L-Defraiteur/lucivy/actions/workflows/ci.yml/badge.svg)](https://github.com/L-Defraiteur/lucivy/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-green)](LICENSE)

BM25 full-text search engine with substring matching, fuzzy search and regex — all
across token boundaries, with exact highlights — in Rust, Python, Node.js, C++ and
the browser.

Built for code search, technical documentation, and as the BM25 side of a vector
database. Everything is MIT.

[**Try the live playground**](https://l-defraiteur.github.io/lucivy/) — it clones
lucivy's own source from GitHub and indexes it in your browser in a few seconds.

![Lucivy playground — the lucivy source indexed in the browser, searching "ror::lucivyer" across token boundaries](docs/25-08-2026/playground_screenshot.jpg)

### What's new in 3.0.0

- **SFX v3** — a new index format: chunked tokens with overlap, a word partition,
  a sibling table, and **exact byte spans** on every query mode, verified one by
  one against `grep` on 50 000 kernel files.
- **Boolean query syntax** (`parse`): `kmalloc AND NOT kfree`, `"exact phrase"`,
  `+must -mustnot`, parentheses — lowered to substring queries with highlights.
- **Fuzzy by Levenshtein or Jaro-Winkler** (`fuzzy_metric`, `min_similarity`):
  a typo at the end of a word now ranks above one at its start.
- **Query warnings**: what the engine will really search, before running it.
- **Bring your own storage (ACID)** in every binding: your object implements
  `load` / `save` / `delete` / `exists` / `list`, lucivy runs on it — a
  transactional database becomes the truth, the mmap cache is disposable.
- **Snapshots served in place**: open a LUCE blob without extracting it.
- **The browser build indexes 10 000 kernel files in 55 s** and answers in
  ~1.5× the native time (it was ~25 minutes and 10×): the engine runs on
  mimalloc, and its memory is bounded by construction.
- One version number for the whole workspace: `ld-lucivy`, `lucivy-core`,
  `luciole`, `lucistore`, `sparse-vector` and the four bindings are all 3.0.3.

Full list: [CHANGELOG.md](CHANGELOG.md). Design: [ARCHITECTURE.md](ARCHITECTURE.md).

## What makes lucivy different

**Substrings, first.** Most search engines match whole tokens: search for `mutex`
and you find the word `mutex` — not `getMutexHandle`, `pthread_mutex_lock` or
`lockmutex`, because the tokenizer sees those as opaque tokens. lucivy matches
**substrings inside tokens**: `mutex` finds every occurrence, buried in compound
words, camelCase identifiers, paths, URLs or concatenated strings, and highlights
exactly the bytes that matched. That is what searching **code** needs — an
identifier fragment, an error message, a config key — and it is where whole-token
engines return nothing.

It works because lucivy builds a **Suffix FST** at indexing time: every suffix of
every token is indexed, partitioned by where it starts (token start, inside a
token, whole word). Substring search becomes as precise as exact-match search,
with BM25 scoring.

- **Across token boundaries, separators included.** Tokenizers split
  `rag3_weaver` into `rag3` and `weaver`; a **sibling table** records who follows
  whom and with which separator, so `rag3weaver`, `rag3_weaver` and
  `rag3-weaver` are all found — separators **relaxed** by default (`_`, `-`,
  `.`, `/`, spaces ignored on both sides), **strict** on request when
  `spin_lock` must not match `spin-lock`. `Error::LucivyError` is found by
  `ror::lucivyer`.
- **Unicode as content.** Accented letters, CJK, **emoji and ZWJ sequences** are
  searchable text like any other and highlighted at their exact bytes — the span
  ground truth is checked against `grep` on files that contain them.
- **Fuzzy with trigram pigeonhole.** At distance *d*, enough trigrams of the
  query must appear exactly; those come from the FST, then the candidate text is
  validated — by **Levenshtein**, or by **Jaro-Winkler** above a similarity, which
  ranks a typo at the end of a word above one at its start. No full scan.
- **Regex by verification.** The required literals of the pattern drive the
  search, `regex::Regex` decides on the rebuilt windows — `spin_lock_[a-z]+`
  costs the price of `spin_lock_`. Patterns with no usable literal fall back to a
  scan, and `query_warnings` tells you so before you run them.
- **Boolean syntax** for humans: `kmalloc AND NOT kfree`, `"exact phrase"`,
  `+must -mustnot`, parentheses — all lowered to substring queries, with
  highlights.
- **BM25 that is correct across shards** — identical scores with 1 or 4 shards
  (diff = 0.0000) — and across machines, through exportable statistics.

## Install

| Language | Install | Package |
|----------|---------|---------|
| Python ≥ 3.9 | `pip install lucivy` | [PyPI](https://pypi.org/project/lucivy/) — one `abi3` wheel |
| Node.js | `npm install lucivy` | [npm](https://www.npmjs.com/package/lucivy) |
| Browser (WASM) | `npm install lucivy-wasm` | [npm](https://www.npmjs.com/package/lucivy-wasm) |
| Rust | `cargo add lucivy-core` | [crates.io](https://crates.io/crates/lucivy-core) |
| C++ | cxx bridge, build from source — [README](bindings/cpp/README.md) | |

Prebuilt binaries are Linux x86_64; everything builds from source elsewhere.

## Quick start

### Python

```python
import lucivy

index = lucivy.Index.create("/tmp/my_index", fields=[
    {"name": "body", "type": "text", "stored": True}
])
index.add(1, body="The pthread_mutex_lock function acquires a mutex")
index.add(2, body="Use std::lock_guard for RAII mutex management")
index.commit()

# Substring — finds "mutex" inside "pthread_mutex_lock", with byte spans
index.search({"type": "contains", "field": "body", "value": "mutex"}, highlights=True)

# Fuzzy — Levenshtein, or Jaro-Winkler above a similarity
index.search({"type": "contains", "field": "body", "value": "mutx", "distance": 1})
index.search({"type": "fuzzy", "field": "body", "value": "mutx",
              "fuzzy_metric": "jaro_winkler", "min_similarity": 0.9})

# Regex — literals drive the search, the regex validates
index.search({"type": "contains", "field": "body", "value": "lock.*mutex", "regex": True})

# Boolean syntax over several fields
index.search({"type": "parse", "fields": ["body"], "value": "mutex AND NOT guard"})

# What will really run
index.query_warnings({"type": "contains", "field": "body", "value": "__init"})
# ['separators are ignored (strict_separators=false): "__init" is searched as "init"']
```

### Node.js

```javascript
const { Index } = require('lucivy');

const index = Index.create('/tmp/my_index', [{ name: 'body', type: 'text', stored: true }]);
index.add(1, { body: 'The pthread_mutex_lock function acquires a mutex' });
index.commit();
index.search({ type: 'contains', field: 'body', value: 'mutex' }, { highlights: true });
```

### Browser

```javascript
import { Lucivy } from 'lucivy-wasm';

const lucivy = new Lucivy('./lucivy-worker.js');   // a Web Worker, pthreads, OPFS
await lucivy.ready;
const index = await lucivy.create('/my-index', { fields: [{ name: 'body', type: 'text' }], shards: 4 });
await index.add(1, { body: 'The pthread_mutex_lock function acquires a mutex' });
await index.commit();
await index.preload();                             // hold the index in memory, once
await index.search({ type: 'contains', field: 'body', value: 'mutex' });
```

### Bring your own storage (ACID)

The index's files are blobs; give lucivy an object that stores them and it runs
on it. A transactional database becomes the source of truth.

```python
class SqliteStore:                      # any object with these five methods
    def load(self, index_name, file_name) -> bytes: ...     # FileNotFoundError when absent
    def save(self, index_name, file_name, data: bytes): ...
    def delete(self, index_name, file_name): ...
    def exists(self, index_name, file_name) -> bool: ...
    def list(self, index_name) -> list[str]: ...
    # optional, for lazy loading: blob_len(...), load_range(..., offset, length)

index = lucivy.Index.create_with_blob_store(SqliteStore("blobs.db"), "acid",
                                            fields=[{"name": "body", "type": "text"}])
```

Same contract in Node.js (`BlobIndex`, asynchronous) and C++ (`lucivy::BlobBackend`).
The store's methods run on lucivy's own threads: thread-safe, and never calling
back into the index.

### Sharded, distributed, synchronised

```python
index = lucivy.Index.create("/tmp/sharded", fields=[...], shards=4)   # parallel search

# Distributed: correct IDF across machines
merged = lucivy.merge_stats([node_a.export_stats(q), node_b.export_stats(q)])
hits = node_a.search_with_global_stats(q, merged, limit=10)

# Snapshots and deltas
blob = index.export_snapshot()                    # LUCE: every shard in one blob
served = lucivy.Index.open_snapshot(blob)         # read-only, nothing extracted
delta = server.export_sharded_delta(client.shard_versions)   # LUCIDS: changed shards only
client.apply_sharded_delta(delta)
```

## Query reference

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `type` | string | required | `contains`, `contains_split`, `startsWith`, `term`, `phrase`, `fuzzy`, `regex`, `parse`, `boolean`, `disjunction_max`, `more_like_this` |
| `field` / `fields` | string / list | required | Field(s) to search |
| `value` | string | required | Text, pattern, or query syntax |
| `distance` | int | 0 | Edit distance for fuzzy (0 = exact); sizes the candidate set for Jaro-Winkler (default 2) |
| `fuzzy_metric` | string | `levenshtein` | `levenshtein` or `jaro_winkler` |
| `min_similarity` | float | 0.9 | Jaro-Winkler acceptance threshold |
| `strict_separators` | bool | false | Relaxed: `_`, `-`, `.`, spaces ignored on both sides; strict: they must match |
| `anchor_start` | bool | false | Match must start a word |
| `exact_match` | bool | false | Match must cover whole words |
| `regex` | bool | false | Treat `value` as a regular expression |
| `filters` | array | none | Non-text filters: `eq`, `ne`, `lt`, `lte`, `gt`, `gte`, `in`, `not_in`, `between`, `starts_with`, `contains` |

| Type | Meaning |
|------|---------|
| `contains` | Substring, fuzzy or regex, across token boundaries — the primary query |
| `contains_split` | Every whitespace-separated word is a `contains`, OR'd |
| `startsWith` / `term` | Substring at the start of a word / covering whole words |
| `phrase` | Adjacent words in order |
| `fuzzy` / `regex` | Aliases for `contains` + `distance` / `+ regex` |
| `parse` | Plain value: OR of `contains` per word × field. Boolean syntax: `AND` / `OR` / `NOT`, quotes, `+` / `-`, parentheses (`NOT` > `AND` > `OR`) |
| `boolean` / `disjunction_max` | Compose sub-queries |
| `more_like_this` | TF-IDF similarity from a reference text |

Every hit carries byte-offset highlights per field. `query_warnings(query)` returns,
without running the search, the honest caveats: separators ignored, a distance
that rewrites most of a short query, a regex that has to scan.

## Performance

Measured on 26 August 2026 (3.0.2), 10 000 files of the Linux kernel source,
4 shards, the same 21-query panel on both sides, identical hit counts (24-core
machine; the browser is Chrome, 8 threads, the index held in memory).

| | native (Rust, mmap) | browser (WASM) |
|---|---|---|
| index on disk | 2 307 MB | 2 880 MB, held in memory |
| indexing | 26.7 s | 40 s |
| `contains kmalloc` | 34-45 ms | 46-100 ms (first query) |
| `contains` relaxed `kmalloc` | 44-47 ms | 26-40 ms |
| `startsWith netdev` | 45-55 ms | 53 ms |
| `phrase return -ENOMEM` | 53 ms | 47 ms |
| `fuzzy kmallc` (d = 1) | 65-67 ms | 62 ms |
| `fuzzy kmalloc` (d = 2) | 424-436 ms | 270 ms |
| `regex spin_lock_[a-z]+` | 147-164 ms | 124 ms |
| `parse kmalloc AND NOT kfree` | 26 ms | 19 ms |
| **panel mean / median** | **75 / 47 ms** | **71-117 / 45-65 ms** |

On 50 000 kernel files, natively (800 segments, no compaction, indexed in
52 s, 8.1 GB on disk): floor 26 ms, `kmalloc` / `spin_lock` / `__init` 28-29 ms,
`include` (36 824 documents, 214 692 spans) 37 ms, fuzzy d = 1 44-106 ms,
d = 2 189 ms, regex 200-211 ms — every count and every span checked against
`grep`.

> These are **substring** queries across token boundaries with BM25 scoring and
> exact spans — most full-text engines return nothing for them. How to run the
> measurements and the span ground truth: [docs/BENCHMARKS.md](docs/BENCHMARKS.md).

## Architecture in one picture

```
Document ─ tokenizer ─┬─ inverted index (postings, term frequencies)
                      ├─ SFX v3: suffix FST + 7 sidecars per field
                      ├─ fast fields
                      └─ doc store

Query ─ FST walk (substring / trigrams / literals) ─ sibling chains across tokens
      ─ validation on the source text (Levenshtein, Jaro-Winkler, regex)
      ─ BM25 with global statistics ─ byte spans
```

Four crates and four bindings: `ld-lucivy` (engine), `lucivy-core` (`ShardedHandle`,
queries, snapshots, storage), `luciole` (actor runtime and DAGs, WASM-safe),
`lucistore` (blob storage, snapshots, deltas), plus `sparse-vector` (a sparse
vector index with WAND pruning on the same storage and sharding). The whole
design — the SFX engine, sharding, memory, the browser — is in
[ARCHITECTURE.md](ARCHITECTURE.md).

## Building from source

```bash
cargo test --lib                                   # engine, ~1 400 tests
cargo test -p lucivy-core --no-fail-fast           # integration
cd bindings/python && bash build.sh                # maturin develop, then .venv/bin/python -m pytest tests
cd bindings/python && bash build-wheel.sh          # abi3 manylinux_2_28 wheel + sdist (docker)
cd bindings/nodejs && npm run build && node test.mjs
cargo test -p lucivy-cpp
bash bindings/emscripten/build.sh                  # emcc, mimalloc, pthreads; playground/pkg/
cd playground && node serve.mjs                    # http://localhost:9877
```

## Heritage

lucivy started as a fork of [tantivy](https://github.com/quickwit-oss/tantivy)
v0.22. The low-level storage layer (segments, postings, doc store, fast fields,
tokenizers, aggregations) still derives from tantivy's codebase. Everything above
it — the SFX engine, the query system, sharding, distribution, snapshots, the
actor runtime, the blob storage, the bindings and the browser build — was
rewritten or built from scratch. Thank you to the tantivy team for a solid
foundation.

`sparse-vector` is original code, MIT, whose design is inspired by Qdrant's
sparse index — see its [NOTICE](sparse_vector/NOTICE).

## License

MIT. See [LICENSE](LICENSE).
