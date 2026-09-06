# lucivy 4.0.2

[![PyPI](https://img.shields.io/pypi/v/lucivy?label=PyPI&color=blue)](https://pypi.org/project/lucivy/)
[![npm](https://img.shields.io/npm/v/lucivy?label=npm&color=cb3837)](https://www.npmjs.com/package/lucivy)
[![npm wasm](https://img.shields.io/npm/v/lucivy-wasm?label=npm%20wasm&color=cb3837)](https://www.npmjs.com/package/lucivy-wasm)
[![crates.io](https://img.shields.io/crates/v/lucivy-core?label=crates.io&color=e6522c)](https://crates.io/crates/lucivy-core)
[![CI](https://github.com/L-Defraiteur/lucivy/actions/workflows/ci.yml/badge.svg)](https://github.com/L-Defraiteur/lucivy/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-green)](LICENSE)

**One index answers every question, and every answer is checked.** Build the
default index once: it answers exact substrings, matches across separators
(`spin_lock`, `spin lock`, `spinlock`), typos that straddle a token boundary,
regular expressions and two-character needles — with BM25 ranking and the exact
bytes of every match — and nothing to configure per question. The ground-truth
harness compares each answer, documents *and* byte spans, to a byte-by-byte scan
of the files (93 983 Linux kernel files, ten query modes, zero mismatches), and
judges Elasticsearch and tantivy by the same scan. A library: in your process,
in your transaction, in your browser. Rust, Python, Node.js, C++, WASM. MIT.

Built for code search, technical documentation, and as the BM25 side of a vector
database.

[**Try the live playground**](https://l-defraiteur.github.io/lucivy/) — it clones
lucivy's own source from GitHub and indexes it in your browser in a few seconds.

![The presentation page running: it clones lucivy's own source from GitHub, indexes 1 171 files in the browser in 3 s, then runs substring, relaxed-separator, fuzzy, emoji, regex and boolean searches with their measured times and exact highlights — ending on a query typed live, two typos away from its match](docs/28-08-2026/images/demo.gif)

*Nothing is pre-recorded in that terminal: it is the page doing the work in a
tab. The last query is typed by hand — `--fuzzy 2 "ShardedHandel"` finds
`ShardedHandle` in 51 ms.*

### What's new in 4.0.0

- **The index is 3.7× smaller.** The whole Linux kernel (93 983 files, 857 MB
  of text): 18 057 MB in 3.0.8, **4 938 MB** in 4.0 (×5.8 the text), **3 344 MB**
  with `derived_in_ram` (×3.9) — the Elasticsearch that does the same work
  writes 3 082 MB. Same answers, same spans, checked against the files. How:
  keys cut at token boundaries, a table of parents instead of FST outputs,
  postings without byte spans (a match's bytes are derived from one checkpoint
  per 16 positions), 28-bit ordinals, block offset tables.
- **`shared_dictionary`**: one dictionary of token texts per **shard** instead
  of one per segment, in generations compacted by a streaming merge — 23 %
  smaller on the kernel, cold queries ×0.8-1.6. **The default since 4.0.0**
  (`shared_dictionary: false` keeps a suffix FST per segment: indexing ×1.5
  faster, an index 23 % bigger). Indexing
  with it costs ×1.5 (the kernel: 107 s against 56): a commit names its
  segments' new texts and returns, a background task merges them into the
  dictionary, and a search waits for that merge by default
  (`dictionary_wait`) so that its cost never depends on when it runs.
- **`derived_in_ram`**: the three derived sidecars of a segment are not written
  but rebuilt byte for byte when the index opens (the kernel opens in 2 s
  instead of 43 ms; no query pays). Off by default; not for the browser.
- **One corpus, one truth**: `benches/compare_engines.sh` builds lucivy,
  Elasticsearch and tantivy on the same files and judges every row by the
  same scan — [the report](docs/compare-engines-2026-09-05.md), and the
  section below.
- **The playground indexes whole repositories** at its prompt: `index mdn`,
  `index linux` (the entire 2.6.0 kernel, 14 032 files, 28 s), `index go`,
  `index typescript` (39 044 files, 33 s), PostgreSQL, CPython, Redis, Git…
  kept in the browser, reopened in seconds.
- **Compatibility contract**: 4.0 opens a 3.0.x index and returns what 3.0.x
  returned (`test_compat_308`, a fixture written by the published 3.0.8 wheel);
  3.0.x does not open a 4.0 index; the first commit in 4.0 converts without
  return.
- One version number for the whole workspace: `ld-lucivy`, `lucivy-core`,
  `luciole`, `lucistore`, `sparse-vector` and the four bindings are all 4.0.2.

3.0.x brought SFX v3 (exact byte spans on every query mode), boolean syntax,
Jaro-Winkler, query warnings, bring-your-own-storage in every binding, snapshots
served in place and the browser build on mimalloc: [CHANGELOG.md](CHANGELOG.md).
Design: [ARCHITECTURE.md](ARCHITECTURE.md).

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

Prebuilt for Linux x86_64 and aarch64 (glibc ≥ 2.28), macOS x86_64 and arm64,
Windows x86_64 — the wheel and the npm package alike; everything builds from
source elsewhere (Rust toolchain needed).

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

### Smaller on disk: the shared dictionary

```python
# One dictionary per shard instead of one per segment: about 20 % less disk
# and RAM, queries slightly slower at cold cache (x1.2 to x1.6 on exact
# queries, fuzzy ones faster), same answers. Fixed at creation; the default
# since 4.0.0 — shared_dictionary=False keeps a suffix FST per segment.
index = lucivy.Index.create("/tmp/compact", fields=[...], shared_dictionary=True)

# Smaller still on disk: the three derived sidecars of each segment (about a
# third of the index) are not written but rebuilt in RAM, byte for byte, when
# the index is opened. Same answers; opening pays the rebuild (never a
# query), the rebuilt structures stay resident. Fixed at creation.
index = lucivy.Index.create("/tmp/compact", fields=[...], shared_dictionary=True, derived_in_ram=True)
```

Node: `Index.create(path, fields, shards, true, true)`; browser and C++:
`shared_dictionary: true` and `derived_in_ram: true` in the config object.

### Sharded, distributed, synchronised

```python
index = lucivy.Index.create("/tmp/sharded", fields=[...], shards=4)   # parallel search

# Distributed: correct IDF across machines, nothing copied or mounted
merged = lucivy.merge_stats([node_a.export_stats(q), node_b.export_stats(q)])
hits = node_a.search_with_global_stats(q, merged, limit=10)
hits = node_a.search_with_global_stats(q, merged, allowed_ids=[3, 7, 11])  # + pre-filter

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

### 93 983 kernel files, and every answer checked

Each row below was compared, document by document **and byte span by byte
span**, to a naive scan of the same files on disk — the "scan" column is how
long that reference took. Nine rows verified, zero mismatches. The queries are
substring, cross-token, fuzzy and regex: a whole-token engine returns nothing
for most of them, so there is no faster answer to compare against, only a
correct one.

| query | mode | documents | spans | lucivy | naive scan |
|---|---|---|---|---|---|
| `mutex_lock` | substring | 5 145 | 20 797 | **18 ms** | 3 597 ms |
| `mutex_lock` | separators relaxed | 5 825 | 22 817 | 11 ms | 3 829 ms |
| `spin_lock` | substring | 6 569 | 34 667 | 11 ms | 3 796 ms |
| `sched` | whole word | 5 284 | 27 881 | 20 ms | 4 413 ms |
| `sched` | substring | **9 289** | 53 211 | 11 ms | 3 766 ms |
| `printk` | start of token | 4 460 | 24 719 | 13 ms | 4 062 ms |
| `schdule` | fuzzy, 1 edit | 5 196 | 18 825 | 44 ms | 11 777 ms |
| `regsiter` | fuzzy, 2 edits | 34 451 | **265 797** | 778 ms | 12 933 ms |
| `spin_lock_[a-z]+` | regex | 5 510 | 24 368 | 233 ms | 435 ms |

Index: **93 983 documents, 4 938 MB** with the shared dictionary — 5.8× the
857 MB of text; 3.0.8 wrote 18 057 MB for the same files.

The two `sched` rows are the point: **5 284** documents contain it as a word,
**9 289** contain it at all — the difference is `sched_clock`, `schedule`,
`sched_domain`. Both counts are exact.

Reproduce it — the harness builds the index, runs the panel and the reference
scan, and fails if any count or span disagrees:

```bash
git clone --depth=1 --branch v7.2 https://github.com/torvalds/linux /tmp/linux-bench
V3_CORPUS=/tmp/linux-bench V3_SFX_VERSION=4 cargo test --release -p lucivy-core \
    --test test_sfx_v3_ground_truth v3_ground_truth_demo -- --ignored --nocapture
```

The numbers above come from a tree at **Linux 7.2** (`Makefile`: 7.2.0,
"Baby Opossum Posse"; copied on 28 August 2026, so at the release tag or a few
commits after it): 93 983 text files after the harness's filter. Another tree
gives other counts; what does not move is the claim itself — the harness fails
on any disagreement between its counts and spans and the scan of *your* files.

5 September 2026 (4.0.0, branch `v4`), four shards, shared dictionary, idle
machine: Intel Core Ultra 7 270K Plus (24 cores), 93 GB RAM, NVMe, Linux 7.2.
Timings are the search itself; recovering each hit's file for the comparison
is the harness's own work and is reported separately. A Jaro-Winkler row runs
in the same panel but is **timed, not verified** — see
[CHANGELOG.md](CHANGELOG.md) for why it has no reference.

### Against Elasticsearch and tantivy — one corpus, one truth, one command

Same 93 983 kernel files, 857 MB of text. Each engine is configured at its
best for substring search, not at its default: Elasticsearch 8.19 with a
trigram analyzer plus a `wildcard` field for regexes, tantivy 0.25 (upstream,
not the fork) with its `NgramTokenizer`. The truth of every row is the same
byte-by-byte scan of the files; a lucivy count is `OK` only when its documents
**and** its byte spans match it. Full report, generated: [docs/compare-engines-2026-09-05.md](docs/compare-engines-2026-09-05.md).

| engine | how it answers a substring | index | × text | indexing |
|---|---|---|---|---|
| Elasticsearch, standard analyzer | it does not (whole words) | 781 MB | ×0.9 | 28 s |
| Elasticsearch, trigrams + `wildcard` | trigram phrases, regex on the wildcard field | 3 082 MB | ×3.6 | 123 s |
| tantivy, default tokenizer | it does not (whole words) | 612 MB | ×0.7 | 1 s |
| tantivy, `NgramTokenizer` | trigram AND, then the stored text re-read to verify (its n-gram positions are all 0) | 680 MB | ×0.8 | 5 s |
| **lucivy 4.0, shared dictionary** | suffix FST, exact spans | **4 926 MB** | **×5.8** | 107 s |
| **lucivy 4.0, shared dictionary + `derived_in_ram`** | suffix FST, exact spans | **3 335 MB** | **×3.9** | 111 s |

On the substring itself all three agree to the document (`mutex_lock` 5 145,
`spin_lock` 6 569, `sched` 9 289 — bold in the report). Where they part:

| asked | truth | lucivy | Elasticsearch | tantivy |
|---|---|---|---|---|
| `spin_lock`, separators relaxed (also `spin lock`, `spin-lock`, `spinlock`) | 9 552 | **9 552**, 23 ms | 6 577 — not with this analyzer: its trigrams carry the underscore | 6 601 — relaxed is the only mode it has: the separator never enters its index |
| `spinlokc`, two edits, across the token boundary | 10 034 | **10 034**, 148 ms | 3 549 — fuzziness compares whole terms | 6 557 — same |
| `spin_lock_[a-z]+`, a regex | 5 510 | **5 510**, 219 ms | 5 440 (wildcard field, 70 short), 480 ms | 0 — terms are already cut |
| `de`, two characters | 93 009 | **93 009**, 7.7 M spans, 561 ms | 0, silently | 0, silently |
| `retur -ENOMEM`, a fuzzy phrase | 14 449 | **14 449**, 30 ms | 14 446 (`span_near`), 24 ms — it does this well | — |
| **where it matched**: `mutex_lock`, 5 145 documents | 20 797 spans | **all 20 797, 15 ms** | `highlight` on the top 200: 179 ms | verifying 5 145 stored texts: 96 ms |
| your index **in your transaction** | — | **yes**: pluggable store, one commit for your rows and the index, rollback included | no: a server next to your database, a synchronisation to write | no: its own directory, its own commit |
| shards and nodes scoring **as one index**, as a library | — | **yes**, asserted by `test_federated_search` | yes, as a cluster | no: one index, one scale of scores |


**What this comparison is, and is not.** Each engine runs the configuration
its own documentation gives for substring search — Elasticsearch a trigram
analyzer plus a `wildcard` field, tantivy its `NgramTokenizer` — and every
count is judged by the same byte-by-byte scan of the files;
`benches/compare_engines.sh` replays it and
`docs/compare-engines-2026-09-05.md` is the report. Where a cell says "not
with this analyzer", a purpose-built analyzer or plugin may well get closer,
at the price of designing it, configuring it and reindexing — such a
configuration is welcome in the report. The point of the table is elsewhere:
**every question in it is answered by lucivy's default index, with nothing to
configure** — exact, relaxed across separators, fuzzy across token boundaries,
regex, two characters, the positions of every match — and each answer is
checked against the files.

The last two rows are observed, not measured: the store contract is five
methods (`load` / `save` / `delete` / `exists` / `list`) that rag3db implements
over Postgres, and the federation test asserts that a document scores the same
on its node under merged statistics as in one index holding everything. What
the other two do better: Elasticsearch answers a plain substring in 3-8 ms to
lucivy's 12-15, tantivy indexes the corpus in seconds, and both run a
term-level fuzzy in a fifth of lucivy's time at two edits (34 451 documents to
their 21 321 and 29 291 — a different question, and the report says so on every
such row). Reproduce it, Elasticsearch optional:

```bash
docker run -d --name lucivy-es -p 9200:9200 -e discovery.type=single-node \
  -e xpack.security.enabled=false -e ES_JAVA_OPTS="-Xms8g -Xmx8g" \
  docker.elastic.co/elasticsearch/elasticsearch:8.19.0
benches/compare_engines.sh /tmp/linux-bench /tmp/lucivy-compare     # writes compare_engines.md
```

### Browser against native

Measured on 5 September 2026 (4.0.0), the whole Linux 2.6.0 kernel, 4 shards,
shared dictionary, the same engine and the same queries on both sides. The
native run is the harness, verified against a byte-by-byte scan of the files it
indexed (it skips a few directories, hence 13 806 against 14 032); the browser
column is the tab's own timings (Chrome, 24-core machine, the index held in
memory, indexed by the playground's `index linux`). Same engine, same answers
per file; the two file sets are not identical, so the counts are not compared.

| | native (Rust, mmap) | browser (WASM) |
|---|---|---|
| files | 13 806 (the harness skips a few directories) | 14 032, 126 MB of text |
| index | 905 MB on disk | 1 089 MB, held in memory |
| indexing | 23 s | 41 s (a commit every 8 MB of text) |
| `mutex_lock`, separators relaxed | 2 ms | 10-18 ms |
| `spin_lock` / `spin_lock_init`, strict | 3 ms | 11-48 ms |
| `fuzzy schdule` (d = 1) | 10 ms | 29-33 ms |
| `fuzzy regsiter` (d = 2) | 128 ms | — |
| `regex spin_lock_[a-z]+` | 52 ms | 113-127 ms |

10 000 files of a modern kernel, index on disk: 3.0.8 wrote 2 307 MB; 4.0
writes 455 MB per segment, 345 MB with the shared dictionary. The browser pays
two to five times the native engine on the same index; both give the same
counts and the same byte spans, checked against a naive scan of the files.

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
