# lucivy — Architecture

*4.0.2, September 2026. Every number in this document was measured; the
commands are in [docs/BENCHMARKS.md](docs/BENCHMARKS.md), the engine comparison
in [docs/compare-engines-2026-09-05.md](docs/compare-engines-2026-09-05.md)
(`benches/compare_engines.sh` regenerates it), and the working notes in
`docs/05-09-2026/`.*

## Overview

lucivy is a BM25 full-text search engine whose **one default index answers every
question** — exact substrings (`mutex` inside `pthread_mutex_lock`), matches
across separators (`spinlock`, `spin_lock`, `spin lock` are one thing), typos
that straddle a token boundary, regular expressions, two-character needles —
with the exact bytes of every match, and **every answer checked** against the
files. Nothing is configured per question: no analyzer to pick, no field to
duplicate, no reindexing to ask something new. In Rust, Python, Node.js, C++ and
the browser. Four properties organise the design:

- **Every answer is checked.** The ground-truth harness compares each query's
  documents *and* byte spans to a byte-by-byte scan of the files — 93 983
  Linux kernel files, nine query modes, zero mismatches — and the same scan
  judges Elasticsearch and tantivy on the same corpus (§ *One corpus, one
  truth* below).
- **The question the others cannot pose**: `spinlock`, `spin_lock` and
  `spin lock` are the same thing (separators relaxed), a typo may straddle a
  separator (`spinlokc` finds `spin_lock`), and a two-character needle still
  answers. Trigram indexes cannot express the first two and answer zero to the
  third.
- **The index lives in your transaction.** Immutable files plus one metadata
  object written last: it maps onto a database table or an object store, the
  commit of your rows and the commit of the index are the same commit, and
  a rollback takes the index with it.
- **A library that shards and federates with the right BM25.** Statistics are
  aggregated before scoring, so N shards give the scores of one index, and two
  independent nodes exchange their statistics to score as one corpus — as a
  library, in-process, in the browser too.

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

Ground truth: on 93 983 kernel files and on rag3db's 4 600, every span of every
query mode is compared to a byte-by-byte scan of the source files, on the fresh
index and on the merged one (`test_sfx_v3_ground_truth`). The harness fails on a
single disagreeing count or span.

### Files of a segment, per text field

| file | content | weight (kernel, 4.0) |
|---|---|---|
| `.sfx` | the suffix FST (keys cut at token boundaries, a table of parents, `own_len` derived from the key) — **one per shard** with the shared dictionary (`dict-<g>.<field>.sfx`), one per segment otherwise | 23 % |
| `.sfxpost` | chunk-level postings: `(document, position)` — **no byte span** since 4.0 (`SFP5`) | 18 % |
| `.word_sfxpost` | word-level postings: `(document, first, last)` and the in-chunk offset of tail entries only (`WSP5`) | 15 % |
| `.word_pos_map` | position → word starting there, span (`WMP3`, 28-bit ordinals) | 15 % |
| `.posmap` | position → ordinal, plus **one byte offset per 16 positions** (`PMP4`): a match's bytes are *derived* from a checkpoint and the tokens' lengths, not stored per posting | 12 % |
| `.sibling_v3` | the sibling table (`SIB4`) | 10 % |
| `.termtexts` | the tokens' text and metadata (`own_len`, separator length, flags) — per shard with the dictionary | 7 % |
| `.gmap` | segment → shard-dictionary ordinal map, with the shard dictionary only (`GMP2`) | — |

Weights over the SFX files of the whole modern kernel (93 983 files, 857 MB of
text: 4 259 MB of SFX files, 4 938 MB with the doc store and the inverted
index — **×5.8 the text**, 52 KB per document; 3.0.8 wrote 18 057 MB, ×21).
`.posmap`, `.word_pos_map` and `.sibling_v3` are **derived**: they hold nothing
the postings and the token metadata do not, and the `derived_in_ram` option
does not write them — the segment reader rebuilds them, byte for byte, when
the index opens (3 344 MB, ×3.9, the open pays ~2 s for the kernel, never a
query). Every reader still opens the previous layouts (`SFP2`-`SFP4`,
`WSP2`-`WSP4`, `PMP3`, `SIB2`-`SIB3`, `.bytemap` ignored), so a 3.0.x index
opens and answers as before; its first commit or merge in 4.0 converts it.
The inverted index keeps term frequencies only: positions and offsets were read
by nothing on a v3 index.

### The shared dictionary (`shared_dictionary`, `sfx_version` 4)

The default since 4.0.0 (`shared_dictionary: false` keeps a suffix FST per
segment). Instead of one suffix FST per segment, each **shard** keeps one
dictionary, in generations: a commit names the texts its segments minted
(each segment writes its own FST of them, on its build thread) and returns,
a background task merges them into a generation, and beyond eight
generations the smallest ones are compacted by a streaming merge of their
FSTs (the kernel: 19 s and 229 MB of RAM, files identical byte for byte to a
from-scratch build); segments carry a `.gmap` from their local ordinals to
the shard's. A search waits for the background merge by default
(`dictionary_wait`), so its cost never depends on when it runs. A query plans
once per shard (the FST walks over the shared dictionary, in parallel), then
scatters per segment. Cold queries pay ×0.8-1.6 against the per-segment
layout (the regex ×1.6: 242 ms on the whole kernel against 112); indexing
×1.5 (the kernel: 107 s against 56); the kernel index is 23 % smaller, 15 440
browser files 25 %.

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

Scoring is standard BM25. Fuzzy hits are **tiered** (`tier × 1000 + bm25`): the
tier is what the verification measured — the edit distance for Levenshtein
(0 for the exact text, -1 one edit away), `-(1 - similarity) × 10` for
Jaro-Winkler — so a closer match always ranks above a more frequent one, and
negative scores are intended. (3.0.0 and 3.0.1 ordered fuzzy hits by BM25
alone — the v3 query computed the tiers but did not hand them to the scorer —
and the Levenshtein tier was a trigram miss count inherited from the
pigeonhole-only pipeline, which read "16 misses" on a one-edit match; both
fixed after 3.0.2. The counts and latencies in this document are unaffected;
no score figure is quoted.) `query_warnings(query)` returns, without running anything, what the
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
`search_with_global_stats` on each node, top-k merge. That call takes the
same DAG as a local search — shards in parallel, top-k bounded per shard,
batching for an index too large for memory — and accepts `allowed_ids`, so a
pre-filter and the federation's statistics compose: the ids decide which
documents are visited, the statistics how they score.

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

| the whole Linux 2.6.0 kernel, 4 shards, shared dictionary | native | browser (`index linux` in the playground) |
|---|---|---|
| files | 13 806 (the harness skips a few directories) | 14 032, 126 MB of text |
| indexing | 23 s | 41 s (a commit every 8 MB of text: segment size, not document count, sets the memory peak) |
| index | 905 MB on disk | 1 089 MB, held in memory |
| `mutex_lock`, separators relaxed | 2 ms | 10-18 ms |
| fuzzy, one edit / regex | 10 ms / 52 ms | 29-33 ms / 113-127 ms |

Same counts and same byte spans on both sides. 10 000 files of a modern kernel:
3.0.8 wrote 2 307 MB, 4.0 writes 455 MB (per-segment) or 345 MB (shared
dictionary). The playground's terminal indexes twelve whole repositories on
demand (`index mdn`, `index linux`, `index typescript` — 39 044 files in 33 s…),
kept in OPFS and reopened in seconds; the ceiling of a tab is about 200 MB of
text.

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

| binding | bridge | 4.0.0 |
|---|---|---|
| Python | PyO3, one `abi3` wheel for CPython ≥ 3.9 | `query_warnings`, `compact`, `wait_merges_quiet`, `index_bytes`, `drop_index`, `open_snapshot`, `create_with_blob_store`, `shared_dictionary=` and `derived_in_ram=` at creation; the GIL is released around every call |
| Node.js | napi-rs | the same, plus the asynchronous `BlobIndex` for user-provided stores (`sharedDictionary`, `derivedInRam` in its options) |
| C++ | cxx, generated header + static lib | the same, plus `lucivy::BlobBackend`; `lucivy_create` takes a full schema object |
| browser | emscripten, `extern "C"`, pthreads over SharedArrayBuffer, OPFS | `memoryStatus`, `preload`, `dropIndex` (through the worker: WASMFS caches what it mounted), startup options (threads, merges, builds, residency), `shared_dictionary` / `derived_in_ram` in `IndexConfig` |
| Rust | `lucivy-core` | everything |

Every binding takes the same JSON `QueryConfig` and returns hits with byte-offset
highlights per field.

## One corpus, one truth: against Elasticsearch and tantivy

`benches/compare_engines.sh <corpus>` builds lucivy's three layouts, Elasticsearch
8.19 configured at its best for substrings (trigram analyzer, `wildcard` field)
and tantivy 0.25 (default and `NgramTokenizer`) on the same files, and judges
every row by the same scan. On the kernel (93 983 files, 857 MB):

| | Elasticsearch, trigrams + wildcard | tantivy, trigrams | lucivy 4.0 |
|---|---|---|---|
| index | 3 082 MB (×3.6) | 680 MB (×0.8) | 4 926 MB (×5.8); 3 335 MB (×3.9) with `derived_in_ram` |
| `spin_lock`, separators relaxed (truth 9 552) | 6 577 — not with this analyzer | 6 601 — relaxed is its only mode | **9 552** |
| `spinlokc`, two edits across the boundary (10 034) | 3 549 | 6 557 | **10 034** |
| `de`, two characters (93 009) | 0, silently | 0, silently | **93 009** |
| where it matched (`mutex_lock`, 5 145 documents) | `highlight` on 200: 179 ms | 5 145 stored texts re-read: 96 ms | **20 797 spans, all, 15 ms** |

tantivy's n-gram tokenizer emits every position as 0, so a trigram phrase there
matches nothing; its honest path is an AND of trigrams then a verification on
the stored text, timed as such. Where they win, also in the report: Elasticsearch
answers a plain substring in 3-8 ms to lucivy's 12-15, tantivy indexes the corpus
in seconds, both run a term-level fuzzy five times faster than lucivy's two-edit
cross-token one. Each engine runs the configuration its documentation gives for
substring search; a purpose-built analyzer may get closer on a row, at the
price of designing, configuring and reindexing — every question in the table is
answered by lucivy's default index, with nothing to configure, and checked
against the files.

## Heritage

lucivy started as a fork of tantivy 0.22; the segment, postings, doc store, fast
field, tokenizer and aggregation layers still derive from it. The SFX engine, the
query system, sharding and distribution, the snapshots, luciole, lucistore, the
storage backends, the bindings and the browser build are original.
