# lucivy — Architecture

## Overview

lucivy is a BM25 full-text search engine built for substring matching across token boundaries. It indexes every suffix of every token into a Suffix FST, enabling queries that traditional engines cannot answer: find "mutex" inside "pthread_mutex_lock", or "ror::lucivyer" matching "Error::LucivyError".

The architecture has three layers:

```
┌─────────────────────────────────────────────────────┐
│  Bindings (Python, Node.js, C++, WASM, Rust)        │
├─────────────────────────────────────────────────────┤
│  lucivy_core — ShardedHandle, query builder,        │
│                BM25 global, snapshot/delta           │
├─────────────────────────────────────────────────────┤
│  ld-lucivy — SFX engine, indexer, segments,         │
│              postings, doc store, schema             │
├─────────────────────────────────────────────────────┤
│  luciole — Actor runtime, DAG execution,            │
│            scheduler, streaming pipelines            │
└─────────────────────────────────────────────────────┘
```

## The SFX Engine

The core innovation. Every token is decomposed into all its suffixes at indexing time. The token `"lucivy"` (6 bytes) produces 6 entries:

```
SI=0:  lucivy      (full token — partition 0x00)
SI=1:  ucivy       (partition 0x01)
SI=2:  civy        (partition 0x01)
SI=3:  ivy         (partition 0x01)
SI=4:  vy          (partition 0x01)
SI=5:  y           (partition 0x01)
```

All entries are stored in a single FST (Finite State Transducer) — a sorted trie with shared prefixes and compressed outputs. The FST is partitioned by a prefix byte:

- **0x00** — SI=0 entries (token starts). Used by `startsWith` / `anchor_start` queries.
- **0x01** — SI>0 entries (substrings). Used by `contains` queries.

Each FST entry stores: `raw_ordinal` (parent token ID), `si` (suffix index), `token_len` (parent token length). This is packed into 64 bits inline, or stored in an OutputTable for multi-parent entries.

### Files per segment

| File | Purpose |
|------|---------|
| `.sfx` | Suffix FST + parent lists + sibling table + GapMap |
| `.sfxpost` | Posting lists mapping suffix ordinals to doc_ids |
| `.termtexts` | Token text storage for cross-token resolution |
| `.gapmap` | Gap-encoded separators between tokens |

### Falling walk — split detection

When searching for a query like `"ivy_co"`, the engine walks the FST byte-by-byte:

1. Enter partition 0x01 (substring)
2. Walk: `i → v → y` — at byte 3, the FST reaches a final node
3. Check: `si(3) + prefix_len(3) == token_len(6)` — the suffix covers the exact end of token `"lucivy"`
4. This is a **split point** — the query can be split here

The split point means the first part of the query matched a suffix that reaches the end of a token. The remaining query bytes need to match the next token.

### Sibling table — cross-token matching

The sibling table records which tokens are adjacent in the original text:

```
ordinal=1 (lucivy) → next_ordinal=2 (core), gap_len=1 ("_")
```

At a split point, the engine:
1. Looks up the sibling table for the matched token's ordinal
2. Checks that the query bytes at the split point match the gap (separator)
3. Continues the FST walk on the next token (entering partition 0x00 for SI=0)

This is how `"ivy_co"` matches `"lucivy_core"` across two tokens.

### Fuzzy search — trigram pigeonhole

Fuzzy search (Levenshtein distance d) uses a pigeonhole strategy:

1. Decompose the query into overlapping trigrams
2. At distance d, at least `len(trigrams) - 3*d` trigrams must appear exactly
3. Search each required trigram via the SFX engine
4. Validate full Levenshtein distance only on candidates

This avoids scanning the entire index. Only documents containing at least one exact trigram are evaluated.

### Regex search — literal extraction

Regex queries like `"lock[a-z]*_init"` are optimized:

1. Extract literal parts from the regex: `"lock"`, `"_init"`
2. Search each literal via the SFX engine
3. Validate the full regex only on candidate documents

No full-index scan. The SFX engine acts as an accelerator.

## Segments and Indexing

### Segment structure

An index is a collection of immutable segments. Each segment contains:

- **Inverted index** — term → posting lists (doc_ids + positions + term frequencies)
- **SFX index** — suffix FST + sfxpost + termtexts + gapmap + sibling table
- **Doc store** — row-oriented compressed storage for stored fields
- **Fast fields** — column-oriented storage for numeric/keyword fields (bitpacked)
- **Fieldnorm** — per-document token counts for BM25 scoring
- **Alive bitset** — tracks which documents are not deleted

Segments are identified by UUIDs. The file format is `segment_id.ext`.

### Indexing pipeline

```
Document
  → Tokenizer (text → tokens)
  → Inverted index writer (postings + positions)
  → SFX writer (suffix FST + sfxpost + sibling table + gapmap)
  → Fast field writer (column values)
  → Doc store writer (compressed row storage)
  → Segment flush to disk
```

Indexing uses lazy commit: mutations set a `dirty` flag, and the next search auto-commits before executing. Explicit `commit()` is also available.

### Merge

Background merge combines multiple small segments into larger ones, eliminating deleted documents and reducing segment count. The merge policy is log-based with configurable thresholds.

In lucivy, merges run through the luciole actor system — no `thread::spawn`.

## luciole — Actor Runtime

A standalone crate providing the concurrency layer. Designed to be WASM-safe (same code runs on native threads and emscripten pthreads).

### Core concepts

- **Actor** — typed message handlers with priority scheduling (Idle → Critical)
- **GenericActor** — dynamic handler registration by type tag (no enum boilerplate)
- **Scheduler** — fixed thread pool, cooperative wait, priority dispatch
- **Envelope** — serialized messages with `local` field for zero-cost local transport

### DAG execution

- **Dag** — directed acyclic graph of computation nodes
- **execute_dag** — topological execution with parallel fan-out per level
- **execute_dag_async** — non-blocking DAG execution via DagExecutor actor
- **BranchNode / GateNode / MergeNode** — control flow nodes
- **Checkpoint** — save/restore DAG progress

### Non-blocking request-reply

- **pipe_to** — send message, get result as message back. Callback registered before send (no race).
- **collect_replies_to** — N:1 gather. Send N requests, get 1 message when all complete.
- **task_pipe_to** — submit CPU work to thread pool, pipe result to actor.

### Streaming

- **StreamDag** — pipeline topology with topological drain. Feed items through a chain of actors.

### Diagnostics

- **WaitGraph** — tracks all inter-thread/inter-actor dependencies. Dump as mermaid or text.
- **ActorActivity** — dynamic labels visible in scheduler dumps.

## Sharding

`ShardedHandle` distributes documents across N shards:

- **`balance_weight=1.0`** (default) — round-robin. Even distribution, fastest indexation.
- **`balance_weight=0.2`** — token-aware. Co-locates documents sharing rare tokens.
- **`balance_weight=0.0`** — pure token-aware. Maximum co-location.

### Cross-shard BM25

BM25 scoring requires global statistics (total docs, total tokens, document frequency per term). In sharded mode, lucivy aggregates statistics from all shards before scoring, so results are **identical** whether you use 1 shard or 4 (measured diff=0.0000).

Implementation: `AggregatedBm25StatsOwned` wraps N searchers and sums their stats via `Bm25StatisticsProvider` trait.

### Distributed search

For multi-machine deployments:

1. Each node calls `export_stats(query)` → serializable `ExportableStats`
2. Coordinator calls `merge_stats([stats_a, stats_b, ...])` → merged global stats
3. Each node calls `search_with_global_stats(query, merged_stats)` → results scored with correct global IDF
4. Coordinator merges top-K results by score

`ExportableStats` includes `total_num_docs`, `total_num_tokens` per field, `doc_freqs` per term, `contains_doc_freqs` (keyed by `field_id:query_text`), and `regex_doc_freqs`.

## Sync and Persistence

### Snapshot formats

| Format | Description |
|--------|-------------|
| **LUCE** | Full snapshot — all shards, schema, segments in one blob |
| **LUCID** | Single-shard incremental delta (only changed segments) |
| **LUCIDS** | Multi-shard incremental delta (only modified shards) |

### Directory trait

| Implementation | Usage |
|----------------|-------|
| `MmapDirectory` | Native — mmap for reads, buffered writes |
| `RamDirectory` | Tests — pure RAM |
| `StdFsDirectory` | WASM — deferred I/O (RAM until terminate) |
| `BlobDirectory` | ACID — pluggable backend (Postgres, S3, etc.) |

## Query System

All text queries route through the SFX engine via a compat layer in `lucivy_core/src/query.rs`:

| Query type | What it does |
|------------|-------------|
| `contains` | Substring match (cross-token). Primary query type. |
| `contains` + `distance` | Fuzzy substring (trigram pigeonhole) |
| `contains` + `regex` | Regex substring (literal extraction + DFA validation) |
| `contains` + `anchor_start` | Prefix match (SI=0 only) |
| `contains` + `exact_match` | Exact whole-token match |
| `contains_split` | Multi-word: split on whitespace, each word as `contains`, OR'd |
| `startsWith` | Alias for `contains` + `anchor_start` |
| `term` | Alias for `contains` + `anchor_start` + `exact_match` |
| `fuzzy` | Alias for `contains` + `distance` |
| `phrase` | Adjacent tokens in order |
| `regex` | Alias for `contains` + `regex` |
| `boolean` | Combine sub-queries with must / should / must_not |
| `disjunction_max` | Best score from sub-queries |
| `more_like_this` | TF-IDF similarity (not SFX-based) |

### Highlights

All query types produce byte-offset highlights via `HighlightSink`. Highlights are computed during scoring — no second pass over stored text. Cross-token matches produce contiguous highlight ranges spanning the matched tokens.

### Filters

Non-text field filters (numeric ranges, equality, membership) are applied as post-filters:

```json
{"field": "category", "op": "eq", "value": "kernel"}
{"field": "score", "op": "gte", "value": 0.5}
{"field": "status", "op": "in", "value": ["active", "review"]}
```

Ops: `eq`, `ne`, `lt`, `lte`, `gt`, `gte`, `in`, `not_in`, `between`, `starts_with`, `contains`.

Pre-filtering by document ID (`allowed_ids`) uses bitmap intersection before scoring.

## Bindings

| Binding | Technology | Bridge |
|---------|-----------|--------|
| Python | PyO3 | Direct Rust → Python |
| Node.js | napi-rs | Direct Rust → JS |
| C++ | CXX | Generated headers + static lib |
| WASM | emscripten | `extern "C"` + pthreads + SharedArrayBuffer |
| Rust | `lucivy-core` | Native |

All bindings expose the same API surface: create, open, add, update, delete, commit, search (with highlights, fields, filters, allowed_ids), snapshot export/import, delta sync, distributed search.

## WASM considerations

- **No `thread::spawn`** — all threading goes through luciole's scheduler
- `docstore_compress_dedicated_thread: false` in WASM
- `StdFsDirectory` buffers in RAM, flushes to OPFS at `terminate()`
- `WRITER_HEAP_SIZE = 15MB` (vs 50MB native)
- `MAXIMUM_MEMORY = 4GB` (32-bit WASM limit)
- pthreads require `Cross-Origin-Opener-Policy: same-origin` + `Cross-Origin-Embedder-Policy: require-corp`
