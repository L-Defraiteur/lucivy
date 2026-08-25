# lucivy — Architecture

*3.0.0, August 2026. Every number in this document was measured; the commands
are in [docs/BENCHMARKS.md](docs/BENCHMARKS.md) and
[docs/25-08-2026/07-knowledge-dump-outils.md](docs/25-08-2026/07-knowledge-dump-outils.md).*

## Overview

lucivy is a BM25 full-text search engine built for **substring matching across
token boundaries**: find `mutex` inside `pthread_mutex_lock`, `ror::lucivyer` in
`Error::LucivyError`, `rag3weaver` in `rag3_weaver` — with the exact bytes that
matched, fuzzily or by regular expression, in Rust, Python, Node.js, C++ and the
browser.

```
┌──────────────────────────────────────────────────────────────────────┐
│  Bindings — Python (PyO3, abi3) · Node.js (napi) · C++ (cxx) ·        │
│             browser (emscripten, pthreads, OPFS) · Rust               │
├──────────────────────────────────────────────────────────────────────┤
│  lucivy-core — ShardedHandle, query builder and compat layer,         │
│                BM25 global statistics, residency, snapshots served,   │
│                storage backends (filesystem, blob store, snapshot)    │
├──────────────────────────────────────────────────────────────────────┤
│  ld-lucivy — the engine: SFX v3, indexer, segments, merges, postings, │
│              doc store, schema, scoring, highlights                   │
├───────────────────────────┬──────────────────────────────────────────┤
│  luciole — actors, DAGs,  │  lucistore — BlobStore contract, LUCE /  │
│  scheduler, wait graph,   │  LUCID / LUCIDS formats, shard router,   │
│  WASM-safe                │  sync                                    │
└───────────────────────────┴──────────────────────────────────────────┘
       sparse-vector — a sparse vector index (WAND) on the same
       storage, router and actor pool, as the vector side of a RAG store
```

## The SFX engine (v3)

### Every suffix of every token, in one FST

At indexing time each token is decomposed into all its suffixes. `lucivy` gives
six entries — `lucivy`, `ucivy`, `civy`, `ivy`, `vy`, `y` — stored in a single
FST (a sorted, prefix-shared trie with compressed outputs), partitioned by a
leading byte:

| partition | holds | serves |
|---|---|---|
| `0x00` | suffix index 0 — the start of a token | `startsWith`, `anchor_start`, `term` |
| `0x01` | suffix index > 0 — inside a token | `contains` anywhere |
| `0x02` | **word-stripped** entries: a whole word, separators removed | cross-word matching in relaxed mode |

Long tokens are cut into **chunks with an overlap** (`equal_chunk` tokenizer), so
the FST stays bounded whatever the token length and a query can be matched across
chunk boundaries. Non-ASCII bytes — accents, CJK, emoji, ZWJ sequences — are
**content** like letters and digits; separators are everything else.

### Crossing token boundaries: falling walk and the sibling table

A query is walked byte by byte in the FST. When the walk reaches the end of a
token's suffix while query bytes remain, that is a **split point**: the rest of
the query must continue in the *next* token of the same document. The **sibling
table** (`.sibling_v3`) records, per token ordinal, which ordinal follows it and
with how many separator bytes between; `contiguous_siblings` (no gap) is the hot
path. The `falling_walk` follows those links, so `ivy_co` matches `lucivy_core`
and `ror::lucivyer` matches `Error::LucivyError`.

**Separators** are the user's choice: *relaxed* (default) strips `_`, `-`, `.`,
`/`, spaces from both the query and the text before comparing, so `rag3weaver`
finds `rag3_weaver`; *strict* requires them to match exactly. In both modes the
highlight covers the real bytes of the text.

### Verification on the source text

Every match is confirmed on the text itself before it is reported — that is what
makes the byte spans exact rather than approximate:

- **contains**: `verify_literal` and `verify_boundaries` re-read the window from
  `.termtexts` + `.posmap` and check the literal and, for `startsWith` / `term`,
  the word boundaries.
- **fuzzy**: candidates come from a **trigram pigeonhole** (at edit distance *d*,
  at least *n − d* trigrams of the query must be present, found by FST walks and
  chained by document and position); each candidate window is then validated by
  **Levenshtein** (a semi-global DP, so the occurrence may start anywhere) or,
  when asked, by **Jaro-Winkler** (`fuzzy_metric`, `min_similarity`) — the
  pigeonhole bounds the cost, the metric decides and ranks.
- **regex**: the pattern's required literals (`regex-syntax`) drive the search;
  `regex::Regex` decides on windows rebuilt around them. A pattern with no
  usable literal scans, and `query_warnings` says so.

Ground truth: on 50 000 kernel files and on rag3db's 4 600, every span of every
query mode is compared to a `grep` of the source files, on the fresh index and
on the merged one (`test_sfx_v3_ground_truth`).

### Files of a segment, per text field

| file | content | weight | touched by one `contains` |
|---|---|---|---|
| `.sfx` | the suffix FST | 44 % | 18-20 % |
| `.bytemap` | 256-bit bitmap of the bytes present per ordinal | 12 % | — |
| `.sfxpost` | chunk-level postings (SFP3) | 9 % | 23 % |
| `.word_sfxpost` | word-level postings (WSP3) | 9 % | 72 % |
| `.termtexts` | the tokens' text | 6 % | 1.5 % |
| `.sibling_v3` | the sibling table (SIB2) | 5 % | 56 % |
| `.posmap` | position → ordinal | 5 % | 47 % |
| `.word_pos_map` | inverse of `.word_sfxpost` | 5 % | 37 % |

Weights from a 15 440-file kernel index (3 392 MB, **220 KB per document**);
"touched" from `mmap` + `mincore` on a `kmalloc` query. The three postings /
sibling files are **delta + varint** encoded with checkpoints where random access
needs them (every 8 documents in `.sfxpost`, 32 in `.word_sfxpost`, none in the
sibling table, which is read sequentially) — 22 % smaller than fixed-width, and
the reader accepts both layouts. The inverted index keeps term frequencies only:
positions and offsets were read by nothing on a v3 index.

### Indexing: bounded by construction

```
document ─ tokenizer ─┬─ inverted index writer (postings, frequencies)
                      ├─ SFX collector v3 (tokens, chunk and word postings, sibling pairs)
                      ├─ fast fields · doc store · fieldnorms
                      └─ segment cut when a budget is hit ─ background build:
                            FST (builder v3) + the 7 sidecars ─ published at commit
```

Three budgets bound a segment and its build: the postings heap
(`LUCIVY_WRITER_HEAP`, a total split across writer threads), the **SFX collector
budget** (`LUCIVY_SFX_HEAP` — the FST builder's peak scales with the segment's
tokens, and nothing bounded it before 3.0.0), and **build permits**
(`LUCIVY_MAX_PENDING_FINALIZE`): a segment build takes a permit before it starts,
with the same cooperative wait as merges — the waiting thread runs other ready
work, so it can never deadlock the actors it depends on. The API queues at most
`LUCIVY_MAX_INFLIGHT_DOCS` documents between the caller and the indexers,
waiting on the caller's own thread. Merges are capped per target
(`LUCIVY_MAX_MERGED_DOCS`) and run one at a time in the browser
(`LUCIVY_MERGE_CONCURRENCY`); `wait_merges_quiet()` is what "nothing is merging"
means — a commit returning never meant that, the policy plans the next round
from what it just published.

## Queries

All text queries go through the SFX engine via a compat layer
(`lucivy_core/src/query.rs`):

| type | what runs |
|---|---|
| `contains` | substring across tokens — the primary query; `distance` makes it fuzzy, `regex` a regex, `anchor_start` / `exact_match` bound it to words |
| `contains_split` | every whitespace-separated word as a `contains`, OR'd |
| `startsWith` · `term` · `phrase` | `contains` anchored at a word start · covering whole words · adjacent words in order |
| `fuzzy` · `regex` | aliases of `contains` with `distance` (Levenshtein or Jaro-Winkler) / `regex` |
| `parse` | plain value: OR of `contains` per word × field; boolean syntax (`AND` / `OR` / `NOT`, quotes, `+` / `-`, parentheses, `NOT` > `AND` > `OR`) lowered to `boolean` over `contains` — highlights in both cases |
| `boolean` · `disjunction_max` | composition |
| `more_like_this` | TF-IDF on the inverted index, the one query that is not SFX |

Scoring is standard BM25. Fuzzy hits are **tiered** (`penalty × 1000 + bm25`): by
trigram misses for Levenshtein, by similarity for Jaro-Winkler — negative scores
are intended. `query_warnings(query)` returns, without running anything, what the
engine will actually do: separators ignored, a distance that rewrites most of a
short query, a regex that has to scan, legacy segments.

### The search DAG

```
drain → flush → [prescan, one node per segment × field] → merge prescans → build weight
                                                                            ↓
                                                  [search per shard] → merge → output
```

The prescan — the FST walks, the chains, the verification — is **one task per
segment**; that is where a query's parallelism lives, and its wall time is that
of its biggest segment. The shard actors only run the scoring phase (a few ms).
BM25 statistics are aggregated over every shard before scoring
(`AggregatedBm25StatsOwned`), so scores are identical with 1 or 4 shards; the
same statistics are serialisable (`ExportableStats`) for a distributed search:
`export_stats` on each node, `merge_stats` on the coordinator,
`search_with_global_stats` on each node, top-k merge.

### Filters

Non-text filters (`eq`, `ne`, `lt`, `lte`, `gt`, `gte`, `in`, `not_in`,
`between`, `starts_with`, `contains`) compose with `must` / `should` /
`must_not`. A pre-filter by document id (`allowed_ids`) is **routed**: only the
shards holding those ids work, each on its own share, and ties are deterministic
(score, then shard / segment / doc).

## Sharding, storage, formats

`ShardedHandle` runs N shards over a `ShardStorage`; documents are routed by a
`ShardRouter` (`balance_weight = 1.0`: round-robin, fastest indexing; `0.2`:
token-aware, co-locates similar documents). Each shard is a `LucivyHandle` over
a `Directory`:

| storage | what it is | when |
|---|---|---|
| `FsShardStorage` / `MmapDirectory` | files on disk, mmap reads, page-granular I/O | native default |
| `StdFsDirectory` | files read whole into a bounded LRU cache, lazy handles, a 4 KB head per file | the browser (OPFS), remote-like stores |
| `BlobShardStorage` / `BlobDirectory` | **blobs are the truth, the mmap cache is disposable**; `BlobLoadMode::Lazy` pulls a file on its first byte read through `blob_len` / `load_range` | ACID over a database or an object store |
| `SnapshotShardStorage` / `SnapshotDirectory` | a LUCE blob served in place: every file a slice of the blob, read-only | serving a packaged index without a copy |

The `BlobStore` contract (`lucistore`) is five methods — `load`, `save`,
`delete`, `exists`, `list` — plus the optional `blob_len` / `load_range` pair for
lazy loading. It is implemented by rag3db over Postgres, and, since 3.0.0, by
whatever object the user hands to the Python (`create_with_blob_store`), Node.js
(`BlobIndex`, asynchronous) or C++ (`lucivy::BlobBackend`) binding. The store's
methods run on lucivy's scheduler threads: thread-safe, never re-entrant, and the
caller's thread must not hold its language's lock while waiting (the GIL is
released around every Python call; the Node API is asynchronous for that reason).

| format | content |
|---|---|
| **LUCE** | full snapshot: root files, then every shard's live files (`name`, `length`, bytes) — a sequential format with a manifest, so it can be **served without extraction** (`open_snapshot`) |
| **LUCID** | one shard's incremental delta (changed segments only) |
| **LUCIDS** | the sharded delta: only the shards that changed, with their `.del` files |

## Memory and the browser

WebAssembly addresses **4 GB**, and everything below follows from it.

- **Residency**: `ShardedHandle::residency()` measures the index (a `u64` sum —
  `usize` is 32 bits on wasm32 and a 5.7 GB index once summed to 1.4) and
  decides: `InMemory` (the cache is raised to the index size, `preload()` reads
  everything once, after merges are quiet) or `Streaming` (shards go through a
  memory budget batch by batch, correct but slow, and `memory_warnings()` says
  so). Default limit 3 GB.
- **A page that indexes cannot also serve** several GB: the index plus what
  indexing leaves behind exceeds the address space (measured: the first search
  fails on a 4 MB allocation). The architecture is three phases — index into
  OPFS, package a LUCE, serve the LUCE — and the playground reloads on the
  persisted index when it is large.
- **The allocator was the WASM factor.** emscripten's default `dlmalloc` takes
  one global lock under pthreads; the query paths that cross token boundaries
  allocate, and four threads serialised on that lock. With `mimalloc` the same
  page went from 551 to 188 ms per query, and 8 scheduler threads finally paid.
  mimalloc keeps freed pages with the thread that freed them, hence two segment
  builds at a time rather than four.
- **Segment size is a query's critical path**: the prescan is one task per
  segment, so 48 segments of ~200 documents fill eight threads where 19 of 2 000
  fed one — merges are capped at 800 documents in the browser (172 → 124-133 ms
  per query on the same index).

| 10 000 kernel files | native | browser |
|---|---|---|
| indexing | 25.7 s | 55 s |
| query, mean / median | 79 / 49 ms | 124-133 / 69-92 ms |
| index | 2 305 MB (compacted) | 2 879 MB, held in memory |

## luciole — the actor runtime

A standalone crate: the same code runs on native threads and on emscripten
pthreads, and **nothing in lucivy calls `thread::spawn`**.

- **Actors** with typed handlers and priorities (Idle → Critical), a fixed
  scheduler pool (`min(cores, 8)` in the browser), cooperative waits that run
  other ready work instead of blocking a thread.
- **DAGs** (`execute_dag`, level-parallel), streaming pipelines (`StreamDag`),
  non-blocking request / reply (`pipe_to`, `collect_replies_to`,
  `task_pipe_to`).
- **One rule, enforced**: a handler never blocks — a cooperative wait inside a
  handler panics. Waits go into tasks (merges and segment builds take their
  permits there) or onto the caller's own thread.
- **WaitGraph** and activity labels: who waits on what, dumped as Mermaid or
  text — the tool that finds a stalled indexer in a browser.

## Bindings

| binding | bridge | 3.0.0 |
|---|---|---|
| Python | PyO3, one `abi3` wheel for CPython ≥ 3.9 | `query_warnings`, `compact`, `wait_merges_quiet`, `index_bytes`, `drop_index`, `open_snapshot`, `create_with_blob_store`; the GIL is released around every call |
| Node.js | napi-rs | the same, plus the asynchronous `BlobIndex` for user-provided stores |
| C++ | cxx, generated header + static lib | the same, plus `lucivy::BlobBackend` |
| browser | emscripten, `extern "C"`, pthreads over SharedArrayBuffer, OPFS | `memoryStatus`, `preload`, startup options (threads, merges, builds, residency) |
| Rust | `lucivy-core` | everything |

Every binding takes the same JSON `QueryConfig` and returns hits with byte-offset
highlights per field.

## Heritage

lucivy started as a fork of tantivy 0.22; the segment, postings, doc store, fast
field, tokenizer and aggregation layers still derive from it. The SFX engine, the
query system, sharding and distribution, the snapshots, luciole, lucistore, the
storage backends, the bindings and the browser build are original.
