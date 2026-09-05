Unreleased (branch v4)
================================

- **`shared_dictionary: true` at creation** (`sfx_version` 4): each distinct
  token text is stored once per shard instead of once per segment — about
  20 % less disk and RAM (Linux kernel: 30 000 files 1 659 → 1 327 MB,
  93 983 files 7.3 → 5.6 GB), same counts, spans and scores, queries ×0.8 to
  ×1.6 at cold cache. Exposed in every binding: Python
  `Index.create(..., shared_dictionary=True)`, Node
  `Index.create(path, fields, shards, true)` / `BlobIndex` option
  `sharedDictionary`, C++ and browser `"shared_dictionary": true` in the
  schema object. Off by default; fixed at creation. A 3.0.x binary does not
  read such an index.
- Index format v4 on the branch: `.sfx` container version 8, ordinals on 28
  bits, block-coded offset tables (`SFP4`, `WSP4`, `SIB4`, `.termtexts`
  layout 3), `.gmap` layout 2. Every reader still opens the previous layouts.
- **The postings no longer store byte spans** (`SFP5`, `WSP5`): a chunk
  posting is a position, a word posting its first and last positions (plus,
  for the tail entry of a word over 264 bytes, its offset within its chunk).
  The byte offset of a position derives from the `.posmap`, which now
  carries one byte checkpoint per sixteen positions (`PMP4`), and the
  tokens' lengths (`own_len`, termtexts META); a token's span is its
  content length. Checked entry by entry against the stored spans of the
  whole Linux kernel before the change (167 M chunk and 137 M word postings,
  no disagreement), then by the ground-truth panels (counts and spans
  against a grep of the disk) after. Spans were 37 % of the postings and
  15 % of an index; the checkpoints cost 0.25 B per position. A segment
  written with spans is still read, and its spans still used. Query
  resolution works in positions end to end; the spans of the matches that
  are kept are placed once, from the posmap.
- **Fixed: the tail entry of a very long word pointed at the wrong
  position.** A word of more than 264 bytes (a line of Chinese, no separator
  inside) gets a second entry for its last bytes; when the word's trailing
  separators spilled into a chunk of their own, that entry's `first_position`
  was the separator-only chunk while its `byte_from` was the chunk before.
  Three occurrences in the 137 million word postings of the Linux kernel
  (its Chinese translations), found by a new consistency check
  (`postings_measure::byte_spans_are_derivable`) — the byte spans, hence the
  highlights, were right; the position, hence `.word_pos_map`, was off by one
  chunk. `preload()` in the browser now reports `merge_wait_ms`, the time
  spent waiting for background merges before reading (70 s after sixteen
  commits of 1 000 kernel files, against 4 s of reading), and the playground
  says so instead of "loading into memory".
- **Fixed: `memory_status()` took a second in the browser.** It re-counted
  the index's bytes by opening every file of every shard on OPFS at each
  call (1 700 files, 0.8 to 1.3 s) instead of using the per-shard count
  `residency()` already memoizes against the segment list. The playground
  calls it after every search to show the truncation flag, and the worker
  serves messages in order, so the next keystroke's search waited behind it:
  the same query showed 60 ms once and 400 ms the next time. Now 6 to 9 ms,
  and the page's time matches the engine's to the millisecond.
- **Browser: two background merges at once for a shared-dictionary index.**
  The browser ran one merge at a time since a v3 merge rebuilds a segment's
  FST in RAM and four at once exhausted the 4 GB address space. A
  dictionary-mode merge rebuilds no FST: measured on 15 440 kernel files in
  4 shards, two or four merges at once left the wasm memory high-water mark
  unchanged (2 539 MB) and cut the wait before the index could be served
  from 74 s to 4 s. `mergeConcurrency` (`--merge-concurrency=N`) forces a
  value for either kind of index; `memoryStatus()` now reports `heap_bytes`,
  the wasm memory high-water mark.
- **Dictionary compaction as a merge of streams.** Past
  `LUCIVY_DICT_MAX_GENERATIONS` (8) live generations, a commit used to read
  every text of the dictionary into RAM and rebuild one FST from all of them
  (Linux kernel, 22.5 million ids: 48 s and 12.8 GB of RAM, at every eighth
  commit). It now merges the smallest generations — enough to halve the
  count — by walking their FSTs together, copying the records of keys held by
  one file byte for byte and merging the others, the output FST streamed to
  disk (`suffix_fst/dictionary_compact.rs`): the same kernel merge takes 19 s
  and 229 MB, and produces byte-identical files. A leftover generation file
  from a crashed commit no longer blocks the next one.

Lucivy 3.0.8
================================

Incremental sync reaches the browser, the path bug that was quietly breaking
snapshot export with it, and the wasm package is finally built by CI. Every
crate and binding ships as **3.0.8**.

- **`lucivy-wasm` is built and published from CI**, on the same tag as
  everything else. It was the one artefact produced on a maintainer's machine
  — not reproducible from a tag, and the only package still needing a one-time
  password. The job pins emsdk 6.0.8, takes a nightly with `rust-src` for
  `-Z build-std`, runs `build.sh` unmodified, and refuses to go on unless the
  output starts with the wasm magic number: a failed link leaves a file that
  exists and weighs something, so checking for a non-empty file proves nothing.

- **A created index kept the wrong path**, and it broke three entry points.
  `lucivy_create` stored the caller's bare path in its context while the index
  was created at `/opfs/lucivy/<path>`; `lucivy_open` stored the prefixed form.
  So every wasm entry point that reaches for files through `ctx.index_path` —
  `lucivy_export_snapshot`, `lucivy_export_sharded_delta`,
  `lucivy_apply_sharded_delta` — looked under `/name` for an index living at
  `/opfs/lucivy/name`. **Snapshot export failed on any index created in the
  session**, reported by the worker as "index may have uncommitted changes",
  which is the message for a null return and not what actually went wrong. An
  index reopened through `lucivy_open` was unaffected, which is why the
  playground never tripped over it.
- **Incremental sync is reachable from the browser.** `shardVersions()`,
  `exportShardedDelta(clientVersions)` and `applyShardedDelta(data)` on
  `LucivyIndex`. The three C entry points had been compiled into the wasm and
  listed in `EXPORTED_FUNCTIONS` from the start, but nothing above them called
  in, so a browser client could only ever take a whole snapshot — which is what
  makes syncing a growing server index to a browser impractical, and the case
  the delta formats exist for. Pinned end to end by
  `playground/test_delta_sync.mjs`: a client bootstraps from a snapshot, the
  server moves ahead, the client asks with its own versions, applies what comes
  back, and then answers the same query with the same hits. An up-to-date
  client gets 209 bytes.
- `build.sh` now copies the JS layer to `playground/js/`, not just the wasm to
  `playground/pkg/`. The two were kept in sync by hand and diverged for an
  afternoon, so the playground exercised an older binding than the one about to
  be published — exactly what the playground is there to catch.
- `.gitignore` had no `node_modules` rule at all, which is how
  `bindings/nodejs/node_modules/@napi-rs/cli` came to be tracked.

Lucivy 3.0.7
================================

**A relaxed fuzzy search was losing documents.** One fix, and the harness that
found it. Every crate and binding ships as **3.0.7**.

- **`auto` could pick a candidate generator that cannot see across token
  boundaries.** Fuzzy candidate generation has two implementations and a cost
  estimate chooses between them. The `pivot` one resolves candidates from
  trigram postings, and those exist only *inside* a token's chunks — so an
  occurrence whose trigrams shared with the query all straddle a separator has
  no posting, is never proposed, and **its document is not returned at all**.
  Not a missing highlight: a missing result, silently. As an index grows the
  estimate tips towards `pivot`, so the loss only appears at scale — measured
  on the Linux kernel, `kvaser_usb_leaf.c` answered exactly on its own and lost
  four of its five occurrences among thirty-one files, and a document whose
  only near-match crosses a boundary came back as zero hits. `auto` now refuses
  `pivot` whenever separators are relaxed, which is a property known before the
  search runs rather than a risk guessed from the data; with strict separators
  the occurrence lies inside a token by definition and the estimate decides as
  before. Present since 23 August (`9866bc1`), shipped in **3.0.2 through
  3.0.6**. It cost nothing to fix: on 93 605 kernel files the correct path was
  already the faster one — `schdule` at distance 1 went 238.1 ms → 223.8, and
  `regsiter` at distance 2 990.0 → 878.9.
  Pinned by `fuzzy_finds_an_occurrence_that_straddles_tokens`, which fails on
  the old path with the count it lost.
- **A demo panel that verifies what it measures**
  (`v3_ground_truth_demo`, ignored). The bug above was invisible to
  `bench_sharding`, whose every row reports "20 hits" because 20 is the result
  cap: it timed an answer nobody had checked. The new panel is the ground-truth
  harness pointed at a whole kernel tree — each row compares the engine's
  documents *and* its byte spans to a naive scan of the files on disk, and
  prints how long that scan took, which is the honest baseline for what the
  index buys. On 93 605 files, idle machine: `mutex_lock` 5 145 documents and
  20 797 spans exact in 137 ms against 8 678 ms of scanning, `sched` as a
  substring 9 327 documents and 53 336 spans in 78 ms, `regsiter` at distance 2
  **267 348 spans exact** in 879 ms, `spin_lock_[a-z]+` 24 368 spans in 180 ms.
  Nine rows verified, zero failures.
- **Jaro-Winkler rows are timed, never verified, and say so.** The panel has no
  reference for that metric: on the Levenshtein path the engine returns every
  span of a candidate window, so a naive scan is comparable span for span; on
  the Jaro-Winkler path it returns the single best substring of each window, so
  what it reports depends on how the index cut the text into windows. A scan
  answering "every substring above the threshold" would disagree by
  construction. Those rows print `n/a` and `NOT VERIFIED`, and count as neither
  pass nor fail.
- Diagnostics: `V3_DIAG_FUZZY_MAX=n` (`0` for all) uncaps the rejected
  candidates `V3_DIAG_FUZZY` prints — five is enough to see the shape, never
  enough to answer "was this occurrence a candidate at all", which is the only
  question worth asking when a span is missing. The summary line also reports
  how many chains fell below the pigeonhole threshold. The ground-truth harness
  now prints the **path** of a missing span, not only its index in the walk:
  the same `doc=N` is a different file from one run to the next.

Lucivy 3.0.6
================================

Two items of the shared-index specification written by the rag3weaver
session (`docs/26-08-2026/…`): the federated mode now takes the same path as
a normal search, and a sparse commit can no longer be cut in half. Then the
sparse index became a list of segments, and its filter nearly free. Every
crate and binding ships as **3.0.6**.

**Compatibility.** `sparse.mmap` moves to format version 3. Versions 1 and 2
still open and are converted by the next commit, so upgrading is transparent
— but the reverse is not: a **3.0.5 binary cannot read an index written by
3.0.6** (`unsupported version: 3`). Nothing else in the release changes a
format; the full-text index, the snapshots and the deltas are untouched.

- **A federated search goes through the search DAG.**
  `search_with_global_stats` — the mode where each node exports its
  statistics, a coordinator merges them, and every node then scores on the
  federation's corpus without anything being copied or mounted — was a
  sequential loop over shards and segments that collected **every** matching
  document into one `Vec` before sorting and truncating: no parallelism, no
  bounded top-k, no `allowed_ids`, no memory batching, and a footprint
  proportional to the number of matches. It now calls the same
  `search_internal` as `search()`: shards in parallel, top-k bounded per
  shard, an index too large for memory streamed shard batch by shard batch,
  and the highlight repair pass. The merged statistics travel to
  `BuildWeightNode` through `DagOpts::global_stats`, where they replace the
  local aggregate and override the document frequencies the local prescan
  counted — the prescan still runs, it is what fills the cache the scorers
  replay.
- **`search_filtered_with_global_stats`** — the pre-filter of
  `search_filtered` under the federation's statistics: the allowed ids decide
  which documents are visited, the statistics how they score. It came for
  free once the mode joined the DAG, and it reaches every binding: Python and
  Node take an `allowed_ids` / `allowedIds` argument on
  `search_with_global_stats`, C++ gains
  `search_filtered_with_global_stats`, and the wasm export takes the id array
  the other filtered search already took.
- Pinned by `test_federated_search.rs`: the union of what two nodes return is
  what one index holding every document returns, **and a document scores the
  same** on its node as in that single index (never asserted before), for
  substring, cross-token, relaxed-separator, fuzzy, regex and boolean
  queries; a filtered federated search is the federated one intersected with
  the allowed ids; the top-k is the k best.
- **A sparse commit is atomic, and its file carries a checksum.**
  `sparse.mmap`, `vectors.bin` and `dims.bin` were written straight onto
  their destination (`File::create`, `fs::write`): an interrupted commit — a
  crash, a full disk — left a truncated index that opened without
  complaining and answered from what was left. They are now written to a
  temporary, flushed, synced and renamed over the destination, with the
  directory synced too. `sparse.mmap` gains a **CRC-32 footer** (format
  version 2; version 1 still opens), `open` checks the length its own
  headers describe, and `MmapPostingData::verify_checksum` (or
  `LUCIVY_SPARSE_VERIFY_CRC=1` at every open) recomputes the checksum.
  Pinned by `test_mmap_durability.rs`.
- `_sparse_config.json` carries a `version` and no longer refuses unknown
  fields: it had `deny_unknown_fields` and no version, so the day a field was
  added the previous release could no longer read the file. A newer version
  is now refused with a sentence that says so.
- **A sparse index is a list of segments.** `sparse.mmap` was rewritten
  whole at every commit, so inserting one vector cost what inserting the
  whole index cost — 320 ms at 200 000 vectors, and growing. A commit now
  writes **one segment holding the delta** and names it in `meta.json`:
  28-33 ms whatever the index holds, flat where it was linear
  (`bench_commit_cost.rs`). A deletion is a tombstone in the segments that
  hold the id, which `seg_<id>.ids` answers by binary search — eight bytes a
  document, read only when something is deleted or updated, and it replaces
  the far larger `sparse_vectors.bin`. A search walks every segment and what
  is still in RAM; past eight segments a commit merges them
  (`LUCIVY_SPARSE_MAX_SEGMENTS`, `0` never) — not for search speed, which
  three runs on an idle machine show unchanged from one segment to a
  hundred, but for the file count (two files and one mapping per segment,
  per shard), the write path and the deleted bytes only a merge reclaims.
- **A dimension is its global token id** (`sparse.mmap` format version 3).
  The dimension header's padding word became the token id and the table is
  sorted by it — same sixteen bytes — so a dimension is looked up by binary
  search instead of through `sparse_dims.bin`, which a version 3 file no
  longer needs at all. What it buys: **merging two segments is a walk over
  two sorted tables**, with nothing remapped and no dictionary rebuilt — and
  since a sparse score is a pure dot product, comparable across corpora,
  merging two *indexes* is the same call. `segments::merge_segments` takes
  `&[&Segment]` from anywhere.
  Versions 1 and 2 still open, and the next commit converts them.
  Pinned by `test_global_dims.rs` and `test_segments.rs` — including the
  trap this surfaced: RAM postings were reloaded by position, which under a
  sorted table hands every dimension someone else's list.
- **A filtered sparse search costs almost nothing, at any size** — when the
  allowed ids are sorted and unique. The set used to be copied, sorted and
  deduplicated at every query, and the large-set path rebuilt a hash set of
  it every time too; a sorted set is now read where it is, and membership is
  a binary search rather than a table that has to be built first. Measured
  on real BGE-M3 vectors, 40 000 documents
  (`tests/bench_filter_selectivity.rs`): a domain of 540 000 ids went from
  6.0 ms a query to **0.22 ms**, and the whole curve flattened — ×0.15 of an
  unfiltered search on 0.1 % of the corpus (the filter *wins*), ×1.3 at
  worst from 1 % to 100 %, where it used to reach ×7.7. An unsorted set
  still pays the copy and the sort, every query.
- `sparse` search over segments passes the allowed ids **down to each
  segment** instead of testing a predicate: the choice between a binary
  search per lane and a walk with a membership test is made per segment
  again, as it was before segmentation. Found by a question from the
  rag3weaver session, not by a test.
- A filtered sparse search is pinned as *the unfiltered one intersected with
  the set* — same documents, same order, same scores to a few units in the
  last place, over both code paths and whatever the ids look like (unsorted,
  duplicated, unknown to the index): `tests/test_filter_truth.rs`. And it is
  unchanged by segmentation or by a merge (`test_segments.rs`).
- **The sparse benches run on real BGE-M3 vectors** — 2 924 documents and
  200 queries produced by the rag3weaver session on burn/Vulkan, read from
  `~/lucivy_bench/sparse/` (or `$LUCIVY_SPARSE_DUMP`), with a 500-document
  extract committed under `sparse_vector/tests/fixtures/` so CI has real
  ones too. Getting one number out of them took three tries, and the first
  two were artefacts that looked solid: ×5.3 on twenty segments, measured on
  dimensions spread uniformly with every weight at 1.0 — a corpus where WAND
  cannot prune at all — and ×7.8 on a hundred, measured on the real vectors
  while the same machine was running the model that produced them. On an
  idle machine, three runs, both corpora: the segment count does not change
  search time. **A benchmark on synthetic data measures the generator, and a
  benchmark on a busy machine measures the load.**
- The default `balance_weight` of an index is 0.2, not the router's 1.0 —
  the two comments diverged and `CLAUDE.md` repeated the wrong one.

Lucivy 3.0.5
================================

Prebuilt for five platforms, a presentation page whose terminal indexes and
searches lucivy's source live, and a search that says when it was
truncated. Every crate and binding ships as **3.0.5**.

- **Prebuilt binaries for Linux x86_64 and aarch64 (glibc ≥ 2.28), macOS
  x86_64 and arm64, Windows x86_64.** A release workflow
  (`.github/workflows/release.yml`) builds the Python `abi3` wheel with
  maturin and the Node.js addon with cargo on each platform (Linux inside
  the manylinux_2_28 image, Intel macOS cross-compiled from the arm64
  runner), runs a create/add/search smoke test where it built, attaches
  everything to the GitHub release, and publishes to PyPI (trusted
  publishing) and npm only after the maintainer approves the `release`
  environment. The npm package `lucivy` no longer carries a binary: it
  depends optionally on `lucivy-linux-x64-gnu`, `lucivy-linux-arm64-gnu`,
  `lucivy-darwin-x64`, `lucivy-darwin-arm64`, `lucivy-windows-x64`, npm
  installs the one for the machine, and `index.js` loads it — or a local
  `npm run build`. Elsewhere `require('lucivy')` says which platforms are
  prebuilt and how to build.
- **The playground opens on a presentation page.** A pitch, two buttons,
  and a terminal that starts by itself: it clones lucivy's source from
  GitHub, indexes it with its real progress, then runs eight searches with
  their measured times and the best span highlighted in the real text — a
  substring starting inside a token and running across five, relaxed
  separators, two typos in a nine-word phrase, an emoji, a regex, boolean
  syntax, the short one, a pre-filter. Then the terminal takes your own
  `lucivy search` (`--fuzzy N`, `--jw`, `--regex`, `--phrase`, `--prefix`,
  `--exact`, `--strict`, `--allowed N`, `--limit N`, `--help`). The old
  playground is a second view of the same page (`#playground`), same
  worker, same index. Below: what you just saw, the numbers, install, how
  it works, honest limits.
- **Truncation is reported.** When a segment's resolution hits
  `LUCIVY_MAX_MATCHES_PER_SEGMENT`, the search now says so instead of
  answering as if complete: `ShardedHandle::last_search_truncated()`, Python
  `index.last_search_truncated`, Node `lastSearchTruncated()`, the wasm
  `memory_status` field `last_search_truncated`, and the playground appends
  "truncated: too many matches…" to the result header. The flag travels from
  the resolver (thread-local, one segment per thread) to the query
  (`Query::prescan_truncated`, `BooleanQuery` propagates) to the search DAG
  (`build_weight` metric) to the handle. Test: `test_truncation_flag.rs`.
- `LUCIVY_MAX_MATCHES_PER_SEGMENT=0` and `LUCIVY_HIGHLIGHT_SPAN_CAP=0` (or
  `unlimited`) disable the caps; the wasm build takes
  `--max-matches-per-segment=N`, the playground `?maxmatches=N`.
- CI: a `ground-truth` job compares counts and spans of a strict / relaxed /
  fuzzy / regex panel with a byte-level grep, on the repository's own source
  — answers only, no timing on shared runners.

Lucivy 3.0.4
================================

A filtered search (`allowed_ids`) is a real pre-filter now — the allowed set
reaches the v3 resolvers, and a regex over ten allowed documents answers in
4 ms instead of 126 — and a stack overflow in the falling walk's look-ahead
that had been aborting a ground-truth suite unnoticed is gone. Every crate
and binding ships as **3.0.4**.

- **A filtered search is a pre-filter now.** `allowed_ids` used to reach only
  the collector: the v3 prescan walked the FST, resolved the postings,
  rebuilt the text, verified and recorded spans for every matching document
  of every segment, and the collector dropped the rest — one hit out of ten
  allowed ids cost exactly the unfiltered query. The allowed set now travels
  to the prescan as a document filter on the segment reader (`doc_filter`,
  a channel separate from the alive bitset) and down to the resolvers
  (`DocFilter`, implemented by `HashSet<u32>` and `AliveBitSet`), which
  never decode the postings of a document outside it; the fuzzy pivot
  generator and the batched search take it too, and only the shards that
  hold an allowed id are prescanned. On the 10 000-file kernel index, 10
  allowed ids: `regex spin_lock_[a-z]+` 126 → 4 ms, `contains_split` 92 → 46,
  `fuzzy d1` 44 → 27, `phrase` 6 → 3; the highlight repair pass of an
  overflowed sink shrinks the same way. Answers and spans are unchanged
  (`test_filtered_search_truth`: filtered = unfiltered ∩ allowed on eleven
  query types, before and after deletions). One documented change: a
  *filtered* search now scores as if the index were the allowed subset —
  its prescan only sees that subset, so `doc_freq` is counted there and N is
  the subset's size (the token count scaled with it, so the average field
  length stays the corpus's). The order of a single-term query is unchanged;
  scores match the unfiltered ones only when every id is allowed. Unfiltered
  searches, with or without deletions, keep their scores. The batched
  (streaming) search keeps the corpus statistics for its filtered pass.
- `bench_filtered_search_cost` (ignored) measures it against
  `/tmp/lucivy_parity_native`.
- **Stack overflow in the falling walk's look-ahead.** `overlap_lookahead`
  (24 August) recursed once per byte of the keys that continue past the
  query; a 3 400-byte separator-free "word" in a corpus (a blob, a minified
  line) overflowed a 2 MB thread stack — `baseline_fuzzy_regex` had been
  aborting with `fatal runtime error: stack overflow` unnoticed, a line no
  `FAILED` grep catches. The walk uses an explicit stack now, same visiting
  order.

Lucivy 3.0.3
================================

Found the same night 3.0.2 went out, on a phone and on a four-token fuzzy
query: the fuzzy tier is now the verified edit distance, the browser build
copes with small devices and with an index left half-written, and a losing
OPFS mount no longer lies. Everything below shipped as **3.0.3** on every
registry.

- Playground on a phone: the 1 100-file demo died at commit, and an index
  opened right after a reload was declared "streamed from storage" at 271 MB
  with advice to index fewer files — the storage was still mounting and the
  byte count was a floor. Small devices (`deviceMemory` ≤ 4 or a mobile UA,
  `?desktop` to override) now run 2 scheduler threads, 1 writer thread, 1
  build, merges of 400 and hold at most 1 GB in memory; the page retries the
  count a few times before showing anything; `memory_warnings` says "N files
  could not be opened yet" instead of the size advice when that is the cause;
  a collapsible **Logs** panel shows the engine's log ring and the console
  where there is no devtools, with a Copy button.
- **Fuzzy tier = verified edit distance.** The Levenshtein tier was the
  trigram miss count of the candidate chain, a leftover of the pigeonhole-only
  pipeline: under `pieces` mode the chain holds the resolved pieces, not the
  n-grams, and `test for barious alter` (one substitution, four tokens) scored
  `-15991` — "16 misses". The tier is now the edit distance the verification
  measured (0 exact, -1 one edit, …), independent of how many token
  boundaries the query crosses; Jaro-Winkler already used its verified
  similarity. Test: `lucivy_core/tests/test_fuzzy_tiers.rs`.
- Playground: a stored demo index whose earlier indexing was interrupted
  (its meta names files that were never written — 24 of 124 on a phone) is
  detected at open and rebuilt instead of served with holes.
- Emscripten: two concurrent OPFS mount attempts (startup and the first
  open) had the loser retry four times and warn "mount failed, in-memory
  FS" over a mounted filesystem; it now sees the winner's flag and stops.

Lucivy 3.0.2
================================

The CI is green again on current stable, the workspace really builds on its
declared MSRV, and one real fix: a one-letter query could take the browser
build down.

- **Highlight sink bounded.** Scorers record the spans of every document they
  verify, not only of the top-k returned, and each span cost a `String` plus
  an allocation; typing `t` over 10 000 kernel files produced tens of
  millions of them and killed the 4 GB WebAssembly heap (`memory allocation
  of 25165824 bytes failed`). Spans are now 12 bytes (interned field, `u32`
  offsets — the postings already carry them as `u32`), and the sink stops at
  `LUCIVY_HIGHLIGHT_SPAN_CAP` (4 M native, 1 M wasm). When it overflows,
  `ShardedHandle` repeats the search restricted to the ids it just returned
  — scores and order come from the first pass, the second only fills the
  top-k's highlights. The v3 weight also stopped handing the sink the spans
  of documents the reader's alive bitset excludes, which is what makes a
  filtered search (and that repair pass) record only what it returns.
  `HighlightSink::{with_cap, overflowed, span_count, clear}` and
  `query::highlight_span_cap()` are public. Test:
  `lucivy_core/tests/test_highlight_cap.rs`.
- **Matches bounded per segment.** The sink was only the first structure to
  give: the v3 resolvers build a 40-byte `MatchV3` per occurrence, and the
  browser index of that corpus has 48 segments. `LUCIVY_MAX_MATCHES_PER_SEGMENT`
  (4 M native, 20 k wasm) stops a segment's resolution there; the query is
  then truncated on that segment — counted by `briques::resolve::truncations()`,
  traced under `LUCIVY_VERBOSE` — instead of the process dying. The 21-query
  parity panel never reaches it (counts identical to native, 114 ms/query);
  typing `t`, `te`, `tes`, `test` in the playground now answers in ~200,
  130, 46, 30 ms — after the full panel, on a 2.9 GB index resident in a
  4 GB heap — where 3.0.1 killed the worker. Surfacing the truncation in the
  search reply is a follow-up (the reply is a bare array). Native and browser
  fuzzy scores can differ in the 4th decimal: `tier * 1000 + bm25` in `f32`
  leaves three digits to BM25 at a tier of -7000; same documents, same order.

- `cargo clippy --lib -- -D warnings` passes on Rust 1.98: 164 public items of
  the v3 engine (`suffix_fst::briques`, `collector_v3`, `builder_v3`, the v3
  query types) now carry doc comments; dead code left by the v3 migration is
  removed (`TokenCaptureV3`, the write-only `doc_values`, unused SFP3
  accessors); a few sites use the cheaper form the lint asked for (one hash
  lookup instead of two in the FST walk memo, `strip_prefix`, `to_vec`).
- `clippy.toml` pins `msrv = "1.85"` so style lints cannot push the code onto
  newer std APIs — which caught `is_multiple_of` (Rust 1.87) in `lucivy-fst`.
- The `zstd-compression` feature's test set compiles again (`IndexSettings`
  gained `sfx_version` in 3.0.0).
- CI: the C++ compile test includes `bindings/cpp/include` (the generated
  header now pulls `lucivy/blob_backend.h`).
- luciole: two tests asserted on the global wait-graph *count* and flaked
  under the parallel test runner; they now check their own edge
  (`wait_graph::contains(id)`, new).
- **Fuzzy score tiers reach the v3 scorer.** `fuzzy_v3` computed them —
  `-(miss count)` under Levenshtein, `-(1 - similarity) * 10` under
  Jaro-Winkler — and `FuzzyQueryV3` dropped them on the floor: the cached
  prescan had no field for them and `SfxWeight` never called
  `with_coverage`, so v3 fuzzy hits were ordered by BM25 alone and the
  Jaro-Winkler test passed only when the physical order happened to agree
  (one run in three did not). `CachedPrescan` carries `coverage`, the scorer
  applies it as `tier * 1000 + bm25` as documented. Counts are unchanged;
  fuzzy ordering now puts the closer match first.
- `export_snapshot` right after `add` could succeed instead of refusing: the
  "uncommitted" flag was set by the shard actor when it wrote the document,
  not when the API accepted it. `ShardedHandle` now marks every shard on
  `add_document` / `delete_by_node_id`; `commit` clears them (the Python
  test `test_export_uncommitted_raises` flaked one run in three).
- Playground: dropped files and `?corpus=` had their own indexing loop and
  skipped the "reload to serve" step above 2 GB that the git clone path had;
  a 10 000-file archive was then served from the page that indexed it, and a
  query typed during a benchmark could take the worker down. Both paths now
  share `indexFiles`.

Lucivy 3.0.1
================================

The 3.0.0 crates went to crates.io a few hours before two engine fixes
landed, and with their 2.x READMEs; the wheels and npm packages of 3.0.0
already carried the fixes. 3.0.1 aligns everything again.

- `BlobDirectory::get_file_handle` self-deadlocked in lazy mode when the store
  could not answer `blob_len` (a `MutexGuard` temporary lived through an
  `if let` body that locked the same mutex); pinned by a core test with a
  store that has no `blob_len`.
- The message of a segment-write failure in a background finalize was
  reduced to "background finalize failed"; the first error now reaches the
  commit's reply — a store that refuses a `save` says why.
- READMEs of `ld-lucivy` and `lucivy-core` rewritten for 3.x (the sharded
  handle, queries, snapshots served in place, bring-your-own storage).

Lucivy 3.0.0
================================

SFX v3 by default, exact spans on every query mode, ACID blob storage with
lazy loading, routed filtered search, a new friend crate `sparse-vector`, and
a browser build that indexes 10 000 kernel files in under a minute and answers
in ~1.5x the native time. Every crate of the workspace ships as **3.0.0**:
`ld-lucivy`, `lucivy-core`, `luciole`, `lucistore`, `sparse-vector`, and the
Python / Node.js / C++ / emscripten bindings.

Previous release: 2.0.x (`ld-lucivy` / `lucivy-core` 2.0.0, `luciole` /
`lucistore` 0.1.0, PyPI 2.0.1, npm 2.0.2).

### SFX v3 (new index format, default)

- **`sfx_version = 3` by default.** A `meta.json` without the field is a v2
  index and keeps working; v2 test harnesses use `Index::create_in_ram_sfx2`.
- 8 sidecars per field (`.sfx`, `.sfxpost`, `.termtexts`, `.posmap`, `.bytemap`,
  `.word_sfxpost`, `.word_pos_map`, `.sibling_v3`); dead artefacts removed.
- **Exact highlights**: contains (strict and relaxed), fuzzy and regex return
  byte spans verified one by one against a grep of the source files (rag3db
  4,600 files, Linux kernel 50,000 files), on the fresh index and on the merged
  one. Reference numbers (kernel 50k, 24 cores): floor 25-27 ms, `include`
  (36,824 docs, 214,692 spans) 46 ms, fuzzy d=1 56-110 ms, d=2 171 ms,
  regex ~190 ms.
- **Fuzzy v3**: real spans, parallel prescan, one FST walk per n-gram,
  candidate generators `ngram` / `pivot` / `pieces` / `auto`.
- **Jaro-Winkler as an optional fuzzy metric**: `fuzzy_metric: "jaro_winkler"`
  with `min_similarity` (default 0.9) validates the pigeonhole's candidates by
  Jaro-Winkler instead of Levenshtein; `distance` then only sizes the
  candidate set (default 2). Hits are tiered by similarity, so a typo at the
  end of a word ranks above one at its start.
- **Regex v3 by verification**: required literals (`regex-syntax`) resolved by
  the contains engine, proven windows, `regex::Regex` decides. Character
  classes and literal-free patterns fall back to a full scan.
- **Merge = fresh**: ordinals interned per (text, form), empty chunks on
  multibyte text fixed, encoding cliffs guarded (u32 parent counters).
- Merge policy consulted at commit, output cap, GC no longer deletes `.sfx`
  files of segments being written.
- Words without a trailing separator (last word of a value) are indexed in
  the word partition; `.termtexts` STATS section is versioned.
- Sticky document dispatch (64 docs per indexer): small commits yield one
  segment per shard instead of one per worker.
- **Denser sidecars, same readers**: `.word_sfxpost` (WSP3), `.sibling_v3`
  (SIB2) and `.sfxpost` (SFP3) are delta + varint encoded with checkpoints
  where random access needs them — 4 339 → 3 392 MB on 15 440 kernel files
  (−22 %, 220 KB per document). Readers accept the previous layouts; nothing
  to migrate. `SegmentMeta::list_files_for(version)` names only the files a
  pipeline version writes.
- **Bounded segments**: the SFX collectors carry a memory estimate and the
  indexer cuts a segment on `LUCIVY_SFX_HEAP` (1 GB native, 128 MB wasm) as
  well as on the postings heap — nothing bounded a v3 segment before. Segment
  builds take a permit (`LUCIVY_MAX_PENDING_FINALIZE`), the API queues at
  most `LUCIVY_MAX_INFLIGHT_DOCS` documents, and the finalize queue no longer
  drops receivers (a commit could publish 1 551 of 2 000 documents).
- Merges capped per target (`LUCIVY_MAX_MERGED_DOCS`: 10 000 native, 800
  wasm); `ShardedHandle::wait_merges_quiet()`; `Residency` (`InMemory` /
  `Streaming`, `LUCIVY_RAM_INDEX_MAX`), `preload()`, `memory_warnings()`;
  a LUCE snapshot can be **served without extraction**
  (`ShardedHandle::open_snapshot`, `SnapshotDirectory`, `read_manifest`);
  snapshot export packs live segments only (28 % dead weight before) from
  one read of `meta.json`.

### Queries

- **`parse`** works again (it was unreachable): a plain value is an OR of
  substring `contains` per word × field; boolean syntax (`AND`/`OR`/`NOT`,
  `+`/`-`, quotes, parentheses) is lowered to `boolean` over `contains`
  (NOT > AND > OR, side-by-side words are OR). Highlights on both shapes.
  The old `QueryParser` path is gone.
- **`query_warnings`**: every query answers with honest warnings (which
  semantics was chosen, what was ignored) — in core and in the 5 bindings.
- `startsWith` / `term` fixed on v3; `LucivyHandle::search` works on v3.
- Fuzzy/regex through `ShardedHandle` prescan all shards with a global doc
  count (same scores as single-shard).

### Sharding, persistence, lifecycle

- **Routed filtered search**: with `allowed_ids`, only the shards holding
  those ids work, each on its own share; ties are deterministic
  (score desc, then shard/segment/doc). `node_ids_of(&results)` and
  `shard_for_node_id(id)` avoid reloading documents.
- **`BlobDirectory` / `BlobShardStorage`**: blobs are the source of truth,
  the mmap cache is disposable. Optional **lazy loading** (`BlobLoadMode::Lazy`,
  ranged remote reads through `BlobStore::load_range` / `blob_len`): open
  reads 3.6 KB instead of 104 KB.
- Commit floor: no fsync of the disposable cache, `.managed.json` written at
  the commit point only — 9 docs / 2 shards: 733 ms → 5.6 ms; reopen after
  commit no longer stalls.
- `_node_id` stamped by `add_document`; `add_document_json` (named fields);
  strict `ShardedConfig` (unknown keys are errors) with tolerant reopen of
  stored configs; `drop_index()`; `close()` stops every actor and the handle
  refuses further calls (`handle is closed`).
- Sharded deltas ship `.del` files and only the changed shards (they were
  full re-sends); writer recreated on apply.
- `impl BlobStore for Arc<T>` (so `Arc<dyn BlobStore>` works everywhere).
- `ShardRouter` moved to `lucistore` (re-exported by `lucivy-core`).

### luciole 3.0.0 (was 0.1.0)

- DAG nodes are taken/put back through a sentinel (no more `ptr::read`), a
  panicking node is a failed node, not a double free.
- `Reply` dropped without an answer warns (`LUCIOLE_REPLY_TRACE=1` for a
  backtrace); `ActorRef::request` returns `Err` instead of hanging;
  `Scheduler::try_wait`, `wait_*_result` variants.
- `Pool::scatter_to(targets, …)` and pools tolerant to workers that left.

### lucistore 3.0.0 (was 0.1.0)

- `ShardRouter` (node-id map, `resync`), `BlobStore::load_range` / `blob_len`,
  `ShardStorage` trait with `FsShardStorage` and `BlobShardStorage`.

### sparse-vector 3.0.0 (new crate)

Inverted index for sparse vectors with WAND pruning, mmap or RAM, filtered
search, `ShardedSparseHandle` on the same router / actor pool / storages as
the full-text index. Original code (MIT), design inspired by Qdrant's sparse
index — see its `NOTICE`.

### Bindings

Native bindings (Python, Node.js, C++) expose what the core gained:
`query_warnings`, `compact(max_docs)`, `wait_merges_quiet()`, `index_bytes()`,
`drop_index()`, and `open_snapshot(bytes)` / `open_snapshot_from(path)` — a
LUCE served in place, read-only, without extraction (the core refuses writes
on it up front, and `close()` no longer tries to commit into it). Filtered
search (`allowed_ids`) was already in all three. The Python wheel is `abi3`
(one wheel for every CPython ≥ 3.9, `manylinux_2_28`). PyPI and npm ship
3.0.0 with this release.

**Bring your own storage (ACID)** in all three: the `BlobStore` contract
(`load`, `save`, `delete`, `exists`, `list`, optional `blob_len` and
`load_range` for lazy loading) is implemented by the user's own object and
lucivy runs on it, blobs being the truth and the mmap cache disposable.
Python: a duck-typed object, `Index.create_with_blob_store` /
`open_with_blob_store`, every binding call now releases the GIL (none did —
a store written in Python would have deadlocked at the first commit); tests
with a dict store and a `sqlite3` store reopened from a second connection.
Node.js: a plain object of callbacks, sync or returning Promises, behind a
new **async** `BlobIndex` class whose core calls run off the JS thread; a
JSON-file store read back by a second `node` process. C++: an abstract
`lucivy::BlobBackend` class in `include/lucivy/blob_backend.h`, a mutex-guarded
in-memory backend as the example, a Postgres sketch in the README. In every
binding: a store that throws makes `commit()` fail with the store's message;
`drop_index` empties the store's namespaces; `lazy` opens pull a fraction of
the bytes and never `load` a large file whole.

Two core defects surfaced by this: a self-deadlock in `BlobDirectory::
get_file_handle` in lazy mode when a store cannot answer `blob_len` (a
`MutexGuard` temporary lived through the `if let` body), and the message of
a segment-write failure lost behind "background finalize failed". Both
fixed, the first pinned by a core test with a store that has no `blob_len`.

### WebAssembly (emscripten) and the playground

Measured on 10 000 Linux kernel files, 8 threads, same page:

- **Indexing in 55 s** (was ~25 min), **124-133 ms per query** (median
  69-92 ms; native 79 / 49), counts identical to native on the 21-query
  panel. The engine now runs on **mimalloc** (`-sMALLOC=mimalloc`): dlmalloc
  serialised every thread on one lock, which was the whole gap — the same
  page went from 551 to 188 ms per query with that single flag.
- Scheduler threads `min(cores, 8)`, pthread pool sized at startup; index
  held in memory up to 3 GB (`LUCIVY_RAM_INDEX_MAX`), preloaded once quiet;
  background merges capped at 800 documents (48 small segments fill eight
  threads where 19 large ones fed one); two segment builds at a time.
- Runtime kept alive after `main`; OPFS mount retried from every entry point
  and given up after two failed rounds; `lucivy_memory_status`,
  `lucivy_preload`; flags `--scheduler-threads`, `--writer-threads`,
  `--max-merged-docs`, `--max-builds`, `--ram-index-max-mb`,
  `--file-cache-mb`, `--verbose`, `--no-opfs`.
- Playground: clones lucivy's own source from GitHub and indexes it in the
  page (983 files, 3 s), `postgres/postgres` as the example repository
  (4 373 files, 13 s); every query mode exposed, Jaro-Winkler included,
  relaxed / strict separators as a selector; no bundled dataset any more.

### Fixed

- Double free under luciole when a scatter task failed (latent since the
  parallel merge), and the panic behind it: fuzzy/regex prescans handed the
  scorer an unsorted doc list.
- `close()` under in-flight merges; `LucivyDeltaExporter` race.
- `StdFsDirectory::atomic_write` truncated before writing (empty `meta.json`
  on reload).
- Lucistore compiles under `-D warnings`.

Lucivy 2.0.0
================================

Major release: SFX engine, cross-token search, sharding, distributed search, delta sync.

### SFX Engine (Suffix FST)

The search engine has been rewritten around the SFX engine. Every suffix of every
indexed token is stored in a partitioned FST, enabling substring matching without
full-index scans. This replaces the previous trigram-based NgramContainsQuery.

- **Suffix FST** (.sfx) — all suffixes partitioned by SI (SI=0 = token start, SI>0 = substring)
- **Cross-token matching** — `falling_walk` + `sibling_table` reconstruct matches across token boundaries
- **Fuzzy** — trigram pigeonhole via RegexContinuationQuery (no full scan)
- **Regex** — literal extraction + SFX lookup + DFA validation (no full scan)
- **Regex character classes** — `[a-z]+`, `\w+`, `[0-9]+` now work correctly

### Query System

- **`contains`** is the primary query type — handles substring, fuzzy, regex, phrase, and prefix
- **`anchor_start`** parameter — constrain to SI=0 (token start only)
- **`exact_match`** parameter — match must cover entire token(s)
- **Compat layer** — legacy query types (`term`, `fuzzy`, `regex`, `phrase`, `startsWith`, `parse`, `phrase_prefix`) automatically route through the SFX engine
- **`contains_split`** — split on whitespace, each word becomes a `contains`, OR'd together

### Sharding

- **ShardedHandle** — N shards with configurable routing
- **`balance_weight`** — 1.0 (round-robin, default) to 0.0 (pure token-aware co-location)
- **BM25 cross-shard** — `ExportableStats` for correct IDF across shards (diff=0.0000 single vs 4-shard)

### Sync & Distribution

- **LUCE** — full snapshot export/import (all shards in one blob)
- **LUCID** — incremental delta sync for a single shard (only changed segments)
- **LUCIDS** — incremental delta sync across multiple shards (only modified shards)
- **Distributed search** — `export_stats` / `merge` / `search_with_global_stats` pipeline for multi-machine BM25

### luciole — Actor Runtime (new crate)

Extracted the actor/scheduler system into a standalone crate `luciole`:

- Actor trait with priority scheduling, GenericActor with dynamic handlers
- DAG execution engine (topological, parallel fan-out, checkpoint/restore, undo)
- StreamDag for streaming pipelines
- Non-blocking request-reply: `pipe_to`, `collect_replies_to`, `task_pipe_to`
- WaitGraph for deadlock diagnostics
- WASM-safe (same code runs native and emscripten)

### Bindings

All 4 bindings now at full feature parity:

- **Python** (PyO3) — `pip install lucivy`
- **Node.js** (NAPI) — `npm install lucivy`
- **C++** (cxx bridge)
- **Emscripten** (WASM) — SharedArrayBuffer + pthreads

Each binding supports: search, highlights, fields, snapshots (export+import),
delta sync (export+apply, sharded), distributed search (export_stats + search_with_global_stats), close.

The wasm-bindgen (single-threaded) binding has been removed — emscripten is the only WASM target.

### WASM

- **Deferred I/O** — FsWriter buffers all writes in RAM, flushes to OPFS at `terminate()` only
- **No `thread::spawn`** in actor handlers — docstore compression, watch callbacks, GC all fixed
- **`WRITER_HEAP_SIZE`** — 15MB (vs 50MB native)

### Scoring

- **Fuzzy scoring tiers** — `miss_penalty * 1000 + bm25_score`. Negative scores are intentional (exact > 1-edit > 2-edit).
- **BM25 cross-shard** — identical results single-shard vs N-shard

### Breaking Changes

- Default `balance_weight` changed from 0.2 to 1.0 (round-robin)
- `startsWith` query type removed — use `contains` with `anchor_start: true`
- wasm-bindgen binding removed — use emscripten
- Fuzzy scores can be negative (by design)

---

Tantivy — the history of the engine lucivy forked
================================

Everything below is the changelog of [tantivy](https://github.com/quickwit-oss/tantivy)
as vendored when lucivy forked it (0.25 and before). The fork's rename had
replaced the name in this text too, links included; it is restored here.

Tantivy 0.25
================================

## Bugfixes
- fix union performance regression in tantivy 0.24 [#2663](https://github.com/quickwit-oss/tantivy/pull/2663)(@PSeitz)
- make zstd optional in sstable [#2633](https://github.com/quickwit-oss/tantivy/pull/2633)(@Parth)
- Fix TopDocs::order_by_string_fast_field for asc order [#2672](https://github.com/quickwit-oss/tantivy/pull/2672)(@stuhood @PSeitz)

## Features/Improvements
- add docs/example and Vec<u32> values to sstable [#2660](https://github.com/quickwit-oss/tantivy/pull/2660)(@PSeitz)
- Add string fast field support to `TopDocs`. [#2642](https://github.com/quickwit-oss/tantivy/pull/2642)(@stuhood)
- update edition to 2024 [#2620](https://github.com/quickwit-oss/tantivy/pull/2620)(@PSeitz)
- Allow optional spaces between the field name and the value in the query parser [#2678](https://github.com/quickwit-oss/tantivy/pull/2678)(@Darkheir)
- Support mixed field types in query parser [#2676](https://github.com/quickwit-oss/tantivy/pull/2676)(@trinity-1686a)
- Add per-field size details [#2679](https://github.com/quickwit-oss/tantivy/pull/2679)(@fulmicoton)

Tantivy 0.24.2
================================
- Fix TopNComputer for reverse order. [#2672](https://github.com/quickwit-oss/tantivy/pull/2672)(@stuhood @PSeitz) 

Affected queries are [order_by_fast_field](https://docs.rs/tantivy/latest/tantivy/collector/struct.TopDocs.html#method.order_by_fast_field) and
[order_by_u64_field](https://docs.rs/tantivy/latest/tantivy/collector/struct.TopDocs.html#method.order_by_u64_field)
for `Order::Asc`

Tantivy 0.24.1
================================
- Fix: bump required rust version to 1.81
  
Tantivy 0.24
================================
Tantivy 0.24 will be backwards compatible with indices created with v0.22 and v0.21. The new minimum rust version will be 1.75. Tantivy 0.23 will be skipped.

#### Bugfixes
- fix potential endless loop in merge [#2457](https://github.com/quickwit-oss/tantivy/pull/2457)(@PSeitz)
- fix bug that causes out-of-order sstable key. [#2445](https://github.com/quickwit-oss/tantivy/pull/2445)(@fulmicoton)
- fix ReferenceValue API flaw [#2372](https://github.com/quickwit-oss/tantivy/pull/2372)(@PSeitz)
- fix `OwnedBytes` debug panic [#2512](https://github.com/quickwit-oss/tantivy/pull/2512)(@b41sh)
- catch panics during merges [#2582](https://github.com/quickwit-oss/tantivy/pull/2582)(@rdettai)
- switch from u32 to usize in bitpacker. This enables multivalued columns larger than 4GB, which crashed during merge before. [#2581](https://github.com/quickwit-oss/tantivy/pull/2581) [#2586](https://github.com/quickwit-oss/tantivy/pull/2586)(@fulmicoton-dd @PSeitz)

#### Breaking API Changes
- remove index sorting [#2434](https://github.com/quickwit-oss/tantivy/pull/2434)(@PSeitz)

#### Features/Improvements
- **Aggregation**
    - Support for cardinality aggregation [#2337](https://github.com/quickwit-oss/tantivy/pull/2337) [#2446](https://github.com/quickwit-oss/tantivy/pull/2446) (@raphaelcoeffic @PSeitz)
    - Support for extended stats aggregation [#2247](https://github.com/quickwit-oss/tantivy/pull/2247)(@giovannicuccu)
    - Add Key::I64 and Key::U64 variants in aggregation to avoid f64 precision issues [#2468](https://github.com/quickwit-oss/tantivy/pull/2468)(@PSeitz)
    - Faster term aggregation fetch terms [#2447](https://github.com/quickwit-oss/tantivy/pull/2447)(@PSeitz)
    - Improve custom order deserialization [#2451](https://github.com/quickwit-oss/tantivy/pull/2451)(@PSeitz)
    - Change AggregationLimits behavior [#2495](https://github.com/quickwit-oss/tantivy/pull/2495)(@PSeitz)
    - lower contention on AggregationLimits [#2394](https://github.com/quickwit-oss/tantivy/pull/2394)(@PSeitz)
    - fix postcard compatibility for top_hits, add postcard test [#2346](https://github.com/quickwit-oss/tantivy/pull/2346)(@PSeitz)
    - reduce top hits memory consumption [#2426](https://github.com/quickwit-oss/tantivy/pull/2426)(@PSeitz)
    - check unsupported parameters top_hits [#2351](https://github.com/quickwit-oss/tantivy/pull/2351)(@PSeitz)
    - Change AggregationLimits to AggregationLimitsGuard [#2495](https://github.com/quickwit-oss/tantivy/pull/2495)(@PSeitz)
    - add support for counting non integer in aggregation [#2547](https://github.com/quickwit-oss/tantivy/pull/2547)(@trinity-1686a)
- **Range Queries**
    - Support fast field range queries on json fields [#2456](https://github.com/quickwit-oss/tantivy/pull/2456)(@PSeitz)
    - Add support for str fast field range query [#2460](https://github.com/quickwit-oss/tantivy/pull/2460) [#2452](https://github.com/quickwit-oss/tantivy/pull/2452) [#2453](https://github.com/quickwit-oss/tantivy/pull/2453)(@PSeitz)
    - modify fastfield range query heuristic [#2375](https://github.com/quickwit-oss/tantivy/pull/2375)(@trinity-1686a)
    - add FastFieldRangeQuery for explicit range queries on fast field (for `RangeQuery` it is autodetected) [#2477](https://github.com/quickwit-oss/tantivy/pull/2477)(@PSeitz)

- add format backwards-compatibility tests [#2485](https://github.com/quickwit-oss/tantivy/pull/2485)(@PSeitz)
- add columnar format compatibility tests [#2433](https://github.com/quickwit-oss/tantivy/pull/2433)(@PSeitz)
- Improved snippet ranges algorithm [#2474](https://github.com/quickwit-oss/tantivy/pull/2474)(@gezihuzi)
- make find_field_with_default return json fields without path [#2476](https://github.com/quickwit-oss/tantivy/pull/2476)(@trinity-1686a)
- Make `BooleanQuery` support `minimum_number_should_match` [#2405](https://github.com/quickwit-oss/tantivy/pull/2405)(@LebranceBW)
- Make `NUM_MERGE_THREADS` configurable [#2535](https://github.com/quickwit-oss/tantivy/pull/2535)(@Barre)

- **RegexPhraseQuery** 
`RegexPhraseQuery` supports phrase queries with regex. E.g. query "b.* b.* wolf" matches "big bad wolf". Slop is supported as well: "b.* wolf"~2 matches "big bad wolf" [#2516](https://github.com/quickwit-oss/tantivy/pull/2516)(@PSeitz)

- **Optional Index in Multivalue Columnar Index** 
For mostly empty multivalued indices there was a large overhead during creation when iterating all docids (merge case). 
This is alleviated by placing an optional index in the multivalued index to mark documents that have values. 
This will slightly increase space and access time. [#2439](https://github.com/quickwit-oss/tantivy/pull/2439)(@PSeitz)

- **Store DateTime as nanoseconds in doc store** DateTime in the doc store was truncated to microseconds previously. This removes this truncation, while still keeping backwards compatibility. [#2486](https://github.com/quickwit-oss/tantivy/pull/2486)(@PSeitz)

- **Performance/Memory**
    - lift clauses in LogicalAst for optimized ast during execution [#2449](https://github.com/quickwit-oss/tantivy/pull/2449)(@PSeitz)
    - Use Vec instead of BTreeMap to back OwnedValue object [#2364](https://github.com/quickwit-oss/tantivy/pull/2364)(@fulmicoton)
    - Replace TantivyDocument with CompactDoc. CompactDoc is much smaller and provides similar performance. [#2402](https://github.com/quickwit-oss/tantivy/pull/2402)(@PSeitz)
    - Recycling buffer in PrefixPhraseScorer [#2443](https://github.com/quickwit-oss/tantivy/pull/2443)(@fulmicoton)

- **Json Type**
    - JSON supports now all values on the root level. Previously an object was required. This enables support for flat mixed types. allow more JSON values, fix i64 special case [#2383](https://github.com/quickwit-oss/tantivy/pull/2383)(@PSeitz)
    - add json path constructor to term [#2367](https://github.com/quickwit-oss/tantivy/pull/2367)(@PSeitz)

- **QueryParser**
    - fix de-escaping too much in query parser [#2427](https://github.com/quickwit-oss/tantivy/pull/2427)(@trinity-1686a)
    - improve query parser [#2416](https://github.com/quickwit-oss/tantivy/pull/2416)(@trinity-1686a)
    - Support field grouping `title:(return AND "pink panther")` [#2333](https://github.com/quickwit-oss/tantivy/pull/2333)(@trinity-1686a)
    - allow term starting with wildcard [#2568](https://github.com/quickwit-oss/tantivy/pull/2568)(@trinity-1686a)

- Exist queries match subpath fields [#2558](https://github.com/quickwit-oss/tantivy/pull/2558)(@rdettai)
- add access benchmark for columnar [#2432](https://github.com/quickwit-oss/tantivy/pull/2432)(@PSeitz)
- extend indexwriter proptests [#2342](https://github.com/quickwit-oss/tantivy/pull/2342)(@PSeitz)
- add bench & test for columnar merging [#2428](https://github.com/quickwit-oss/tantivy/pull/2428)(@PSeitz)
- Change in Executor API [#2391](https://github.com/quickwit-oss/tantivy/pull/2391)(@fulmicoton)
- Removed usage of num_cpus [#2387](https://github.com/quickwit-oss/tantivy/pull/2387)(@fulmicoton)
- use bingang for agg and stacker benchmark [#2378](https://github.com/quickwit-oss/tantivy/pull/2378)[#2492](https://github.com/quickwit-oss/tantivy/pull/2492)(@PSeitz) 
- cleanup top level exports [#2382](https://github.com/quickwit-oss/tantivy/pull/2382)(@PSeitz)
- make convert_to_fast_value_and_append_to_json_term pub [#2370](https://github.com/quickwit-oss/tantivy/pull/2370)(@PSeitz)
- remove JsonTermWriter [#2238](https://github.com/quickwit-oss/tantivy/pull/2238)(@PSeitz)
- validate sort by field type [#2336](https://github.com/quickwit-oss/tantivy/pull/2336)(@PSeitz)
- Fix trait bound of StoreReader::iter [#2360](https://github.com/quickwit-oss/tantivy/pull/2360)(@adamreichold)
- remove read_postings_no_deletes [#2526](https://github.com/quickwit-oss/tantivy/pull/2526)(@PSeitz)

Tantivy 0.22.1
================================
- Fix TopNComputer for reverse order. [#2672](https://github.com/quickwit-oss/tantivy/pull/2672)(@stuhood @PSeitz) 

Affected queries are [order_by_fast_field](https://docs.rs/tantivy/latest/tantivy/collector/struct.TopDocs.html#method.order_by_fast_field) and
[order_by_u64_field](https://docs.rs/tantivy/latest/tantivy/collector/struct.TopDocs.html#method.order_by_u64_field)
for `Order::Asc`

Tantivy 0.22
================================

Tantivy 0.22 will be able to read indices created with Tantivy 0.21.

#### Bugfixes
- Fix null byte handling in JSON paths (null bytes in json keys caused panic during indexing) [#2345](https://github.com/quickwit-oss/tantivy/pull/2345)(@PSeitz)
- Fix bug that can cause `get_docids_for_value_range` to panic. [#2295](https://github.com/quickwit-oss/tantivy/pull/2295)(@fulmicoton)
- Avoid 1 document indices by increase min memory to 15MB for indexing [#2176](https://github.com/quickwit-oss/tantivy/pull/2176)(@PSeitz)
- Fix merge panic for JSON fields [#2284](https://github.com/quickwit-oss/tantivy/pull/2284)(@PSeitz)
- Fix bug occurring when merging JSON object indexed with positions. [#2253](https://github.com/quickwit-oss/tantivy/pull/2253)(@fulmicoton)
- Fix empty DateHistogram gap bug [#2183](https://github.com/quickwit-oss/tantivy/pull/2183)(@PSeitz)
- Fix range query end check (fields with less than 1 value per doc are affected) [#2226](https://github.com/quickwit-oss/tantivy/pull/2226)(@PSeitz)
- Handle exclusive out of bounds ranges on fastfield range queries [#2174](https://github.com/quickwit-oss/tantivy/pull/2174)(@PSeitz)

#### Breaking API Changes
- rename ReloadPolicy onCommit to onCommitWithDelay [#2235](https://github.com/quickwit-oss/tantivy/pull/2235)(@giovannicuccu)
- Move exports from the root into modules [#2220](https://github.com/quickwit-oss/tantivy/pull/2220)(@PSeitz)
- Accept field name instead of `Field` in FilterCollector [#2196](https://github.com/quickwit-oss/tantivy/pull/2196)(@PSeitz)
- remove deprecated IntOptions and DateTime [#2353](https://github.com/quickwit-oss/tantivy/pull/2353)(@PSeitz)

#### Features/Improvements
- Tantivy documents as a trait: Index data directly without converting to tantivy types first [#2071](https://github.com/quickwit-oss/tantivy/pull/2071)(@ChillFish8)
- encode some part of posting list as -1 instead of direct values (smaller inverted indices) [#2185](https://github.com/quickwit-oss/tantivy/pull/2185)(@trinity-1686a)
- **Aggregation**
  - Support to deserialize f64 from string [#2311](https://github.com/quickwit-oss/tantivy/pull/2311)(@PSeitz)
  - Add a top_hits aggregator [#2198](https://github.com/quickwit-oss/tantivy/pull/2198)(@ditsuke)
  - Support bool type in term aggregation [#2318](https://github.com/quickwit-oss/tantivy/pull/2318)(@PSeitz)
  - Support ip addresses in term aggregation [#2319](https://github.com/quickwit-oss/tantivy/pull/2319)(@PSeitz)
  - Support date type in term aggregation [#2172](https://github.com/quickwit-oss/tantivy/pull/2172)(@PSeitz)
  - Support escaped dot when addressing field [#2250](https://github.com/quickwit-oss/tantivy/pull/2250)(@PSeitz)

- Add ExistsQuery to check documents that have a value [#2160](https://github.com/quickwit-oss/tantivy/pull/2160)(@imotov)
- Expose TopDocs::order_by_u64_field again [#2282](https://github.com/quickwit-oss/tantivy/pull/2282)(@ditsuke)

- **Memory/Performance**
  - Faster TopN: replace BinaryHeap with TopNComputer [#2186](https://github.com/quickwit-oss/tantivy/pull/2186)(@PSeitz)
  - reduce number of allocations during indexing [#2257](https://github.com/quickwit-oss/tantivy/pull/2257)(@PSeitz)
  - Less Memory while indexing: docid deltas while indexing [#2249](https://github.com/quickwit-oss/tantivy/pull/2249)(@PSeitz)
  - Faster indexing: use term hashmap in fastfield [#2243](https://github.com/quickwit-oss/tantivy/pull/2243)(@PSeitz)
  - term hashmap remove copy in is_empty, unused unordered_id [#2229](https://github.com/quickwit-oss/tantivy/pull/2229)(@PSeitz)
  - add method to fetch block of first values in columnar [#2330](https://github.com/quickwit-oss/tantivy/pull/2330)(@PSeitz)
  - Faster aggregations: add fast path for full columns in fetch_block [#2328](https://github.com/quickwit-oss/tantivy/pull/2328)(@PSeitz)
  - Faster sstable loading: use fst for sstable index [#2268](https://github.com/quickwit-oss/tantivy/pull/2268)(@trinity-1686a)

- **QueryParser**
  - allow newline where we allow space in query parser [#2302](https://github.com/quickwit-oss/tantivy/pull/2302)(@trinity-1686a)
  - allow some mixing of occur and bool in strict query parser [#2323](https://github.com/quickwit-oss/tantivy/pull/2323)(@trinity-1686a)
  - handle * inside term in lenient query parser [#2228](https://github.com/quickwit-oss/tantivy/pull/2228)(@trinity-1686a)
  - add support for exists query syntax in query parser [#2170](https://github.com/quickwit-oss/tantivy/pull/2170)(@trinity-1686a)
- Add shared search executor [#2312](https://github.com/quickwit-oss/tantivy/pull/2312)(@MochiXu)
- Truncate keys to u16::MAX in term hashmap [#2299](https://github.com/quickwit-oss/tantivy/pull/2299)(@PSeitz)
- report if a term matched when warming up posting list [#2309](https://github.com/quickwit-oss/tantivy/pull/2309)(@trinity-1686a)
- Support json fields in FuzzyTermQuery [#2173](https://github.com/quickwit-oss/tantivy/pull/2173)(@PingXia-at)
- Read list of fields encoded in term dictionary for JSON fields [#2184](https://github.com/quickwit-oss/tantivy/pull/2184)(@PSeitz)
- add collect_block to BoxableSegmentCollector [#2331](https://github.com/quickwit-oss/tantivy/pull/2331)(@PSeitz)
- expose collect_block buffer size [#2326](https://github.com/quickwit-oss/tantivy/pull/2326)(@PSeitz)
- Forward regex parser errors [#2288](https://github.com/quickwit-oss/tantivy/pull/2288)(@adamreichold)
- Make FacetCounts defaultable and cloneable. [#2322](https://github.com/quickwit-oss/tantivy/pull/2322)(@adamreichold)
- Derive Debug for SchemaBuilder [#2254](https://github.com/quickwit-oss/tantivy/pull/2254)(@GodTamIt)
- add missing inlines to tantivy options [#2245](https://github.com/quickwit-oss/tantivy/pull/2245)(@PSeitz)

Tantivy 0.21.1
================================
#### Bugfixes
- Range queries on fast fields with less values on that field than documents had an invalid end condition, leading to missing results. [#2226](https://github.com/quickwit-oss/tantivy/issues/2226)(@appaquet @PSeitz)
- Increase the minimum memory budget from 3MB to 15MB to avoid single doc segments (API fix). [#2176](https://github.com/quickwit-oss/tantivy/issues/2176)(@PSeitz)

Tantivy 0.21
================================
#### Bugfixes
- Fix track fast field memory consumption, which led to higher memory consumption than the budget allowed during indexing [#2148](https://github.com/quickwit-oss/tantivy/issues/2148)[#2147](https://github.com/quickwit-oss/tantivy/issues/2147)(@PSeitz)
- Fix a regression from 0.20 where sort index by date wasn't working anymore [#2124](https://github.com/quickwit-oss/tantivy/issues/2124)(@PSeitz)
- Fix getting the root facet on the `FacetCollector`. [#2086](https://github.com/quickwit-oss/tantivy/issues/2086)(@adamreichold)
- Align numerical type priority order of columnar and query. [#2088](https://github.com/quickwit-oss/tantivy/issues/2088)(@fmassot)
#### Breaking Changes
- Remove support for Brotli and Snappy compression [#2123](https://github.com/quickwit-oss/tantivy/issues/2123)(@adamreichold)
#### Features/Improvements
- Implement lenient query parser [#2129](https://github.com/quickwit-oss/tantivy/pull/2129)(@trinity-1686a)
- order_by_u64_field and order_by_fast_field allow sorting in ascending and descending order [#2111](https://github.com/quickwit-oss/tantivy/issues/2111)(@naveenann)
- Allow dynamic filters in text analyzer builder [#2110](https://github.com/quickwit-oss/tantivy/issues/2110)(@fulmicoton @fmassot)
- **Aggregation**
  - Add missing parameter for term aggregation [#2149](https://github.com/quickwit-oss/tantivy/issues/2149)[#2103](https://github.com/quickwit-oss/tantivy/issues/2103)(@PSeitz)
  - Add missing parameter for percentiles [#2157](https://github.com/quickwit-oss/tantivy/issues/2157)(@PSeitz)
  - Add missing parameter for stats,min,max,count,sum,avg [#2151](https://github.com/quickwit-oss/tantivy/issues/2151)(@PSeitz)
  - Improve aggregation deserialization error message [#2150](https://github.com/quickwit-oss/tantivy/issues/2150)(@PSeitz)
  - Add validation for type Bytes to term_agg [#2077](https://github.com/quickwit-oss/tantivy/issues/2077)(@PSeitz)
  - Alternative mixed field collection [#2135](https://github.com/quickwit-oss/tantivy/issues/2135)(@PSeitz)
- Add missing query_terms impl for TermSetQuery. [#2120](https://github.com/quickwit-oss/tantivy/issues/2120)(@adamreichold)
- Minor improvements to OwnedBytes [#2134](https://github.com/quickwit-oss/tantivy/issues/2134)(@adamreichold)
- Remove allocations in split compound words [#2080](https://github.com/quickwit-oss/tantivy/issues/2080)(@PSeitz)
- Ngram tokenizer now returns an error with invalid arguments [#2102](https://github.com/quickwit-oss/tantivy/issues/2102)(@fmassot)
- Make TextAnalyzerBuilder public [#2097](https://github.com/quickwit-oss/tantivy/issues/2097)(@adamreichold)
- Return an error when tokenizer is not found while indexing [#2093](https://github.com/quickwit-oss/tantivy/issues/2093)(@naveenann)
- Delayed column opening during merge [#2132](https://github.com/quickwit-oss/tantivy/issues/2132)(@PSeitz)

Tantivy 0.20.2
================================
- Align numerical type priority order on the search side.  [#2088](https://github.com/quickwit-oss/tantivy/issues/2088) (@fmassot)
- Fix is_child_of function not considering the root facet. [#2086](https://github.com/quickwit-oss/tantivy/issues/2086) (@adamreichhold)

Tantivy 0.20.1
================================
- Fix building on windows with mmap [#2070](https://github.com/quickwit-oss/tantivy/issues/2070) (@ChillFish8)

Tantivy 0.20
================================
#### Bugfixes
- Fix phrase queries with slop (slop supports now transpositions, algorithm that carries slop so far for num terms > 2) [#2031](https://github.com/quickwit-oss/tantivy/issues/2031)[#2020](https://github.com/quickwit-oss/tantivy/issues/2020)(@PSeitz)
- Handle error for exists on MMapDirectory [#1988](https://github.com/quickwit-oss/tantivy/issues/1988) (@PSeitz)
- Aggregation
  - Fix min doc_count empty merge bug [#2057](https://github.com/quickwit-oss/tantivy/issues/2057) (@PSeitz)
  - Fix: Sort order for term aggregations (sort order on key was inverted) [#1858](https://github.com/quickwit-oss/tantivy/issues/1858) (@PSeitz)

#### Features/Improvements
- Add PhrasePrefixQuery [#1842](https://github.com/quickwit-oss/tantivy/issues/1842) (@trinity-1686a)
- Add `coerce` option for text and numbers types (convert the value instead of returning an error during indexing) [#1904](https://github.com/quickwit-oss/tantivy/issues/1904) (@PSeitz)
- Add regex tokenizer [#1759](https://github.com/quickwit-oss/tantivy/issues/1759)(@mkleen)
- Move tokenizer API to separate crate. Having a separate crate with a stable API will allow us to use tokenizers with different tantivy versions. [#1767](https://github.com/quickwit-oss/tantivy/issues/1767) (@PSeitz)
- **Columnar crate**: New fast field handling (@fulmicoton @PSeitz) [#1806](https://github.com/quickwit-oss/tantivy/issues/1806)[#1809](https://github.com/quickwit-oss/tantivy/issues/1809)
  - Support for fast fields with optional values. Previously tantivy supported only single-valued and multi-value fast fields. The encoding of optional fast fields is now very compact.
  - Fast field Support for JSON (schemaless fast fields). Support multiple types on the same column. [#1876](https://github.com/quickwit-oss/tantivy/issues/1876) (@fulmicoton)
  - Unified access for fast fields over different cardinalities.
  - Unified storage for typed and untyped fields.
  - Move fastfield codecs into columnar. [#1782](https://github.com/quickwit-oss/tantivy/issues/1782) (@fulmicoton)
  - Sparse dense index for optional values [#1716](https://github.com/quickwit-oss/tantivy/issues/1716) (@PSeitz)
  - Switch to nanosecond precision in DateTime fastfield [#2016](https://github.com/quickwit-oss/tantivy/issues/2016) (@PSeitz)
- **Aggregation**
  - Add `date_histogram` aggregation (only `fixed_interval` for now) [#1900](https://github.com/quickwit-oss/tantivy/issues/1900) (@PSeitz)
  - Add `percentiles` aggregations [#1984](https://github.com/quickwit-oss/tantivy/issues/1984) (@PSeitz)
  - [**breaking**] Drop JSON support on intermediate agg result (we use postcard as format in `quickwit` to send intermediate results) [#1992](https://github.com/quickwit-oss/tantivy/issues/1992) (@PSeitz)
  - Set memory limit in bytes for aggregations after which they abort (Previously there was only the bucket limit) [#1942](https://github.com/quickwit-oss/tantivy/issues/1942)[#1957](https://github.com/quickwit-oss/tantivy/issues/1957)(@PSeitz)
  - Add support for u64,i64,f64 fields in term aggregation [#1883](https://github.com/quickwit-oss/tantivy/issues/1883) (@PSeitz)
  - Allow histogram bounds to be passed as Rfc3339 [#2076](https://github.com/quickwit-oss/tantivy/issues/2076) (@PSeitz)
  - Add count, min, max, and sum aggregations [#1794](https://github.com/quickwit-oss/tantivy/issues/1794) (@guilload)
  - Switch to Aggregation without serde_untagged => better deserialization errors. [#2003](https://github.com/quickwit-oss/tantivy/issues/2003) (@PSeitz)
  - Switch to ms in histogram for date type (ES compatibility) [#2045](https://github.com/quickwit-oss/tantivy/issues/2045) (@PSeitz)
  - Reduce term aggregation memory consumption [#2013](https://github.com/quickwit-oss/tantivy/issues/2013) (@PSeitz)
  - Reduce agg memory consumption: Replace generic aggregation collector (which has a high memory requirement per instance) in aggregation tree with optimized versions behind a trait.
  - Split term collection count and sub_agg (Faster term agg with less memory consumption for cases without sub-aggs) [#1921](https://github.com/quickwit-oss/tantivy/issues/1921) (@PSeitz)
  - Schemaless aggregations: In combination with stacker tantivy supports now schemaless aggregations via the JSON type.
    - Add aggregation support for JSON type [#1888](https://github.com/quickwit-oss/tantivy/issues/1888) (@PSeitz)
    - Mixed types support on JSON fields in aggs [#1971](https://github.com/quickwit-oss/tantivy/issues/1971) (@PSeitz)
  - Perf: Fetch blocks of vals in aggregation for all cardinality [#1950](https://github.com/quickwit-oss/tantivy/issues/1950) (@PSeitz)
  - Allow histogram bounds to be passed as Rfc3339 [#2076](https://github.com/quickwit-oss/tantivy/issues/2076) (@PSeitz)
- `Searcher` with disabled scoring via `EnableScoring::Disabled` [#1780](https://github.com/quickwit-oss/tantivy/issues/1780) (@shikhar)
- Enable tokenizer on json fields [#2053](https://github.com/quickwit-oss/tantivy/issues/2053) (@PSeitz)
- Enforcing "NOT" and "-" queries consistency in UserInputAst [#1609](https://github.com/quickwit-oss/tantivy/issues/1609) (@bazhenov)
- Faster indexing
  - Refactor tokenization pipeline to use GATs [#1924](https://github.com/quickwit-oss/tantivy/issues/1924) (@trinity-1686a)
  - Faster term hash map [#2058](https://github.com/quickwit-oss/tantivy/issues/2058)[#1940](https://github.com/quickwit-oss/tantivy/issues/1940) (@PSeitz)
  - tokenizer-api: reduce Tokenizer allocation overhead [#2062](https://github.com/quickwit-oss/tantivy/issues/2062) (@PSeitz)
  - Refactor vint [#2010](https://github.com/quickwit-oss/tantivy/issues/2010) (@PSeitz)
- Faster search
  - Work in batches of docs on the SegmentCollector (Only for cases without score for now) [#1937](https://github.com/quickwit-oss/tantivy/issues/1937) (@PSeitz)
  - Faster fast field range queries using SIMD [#1954](https://github.com/quickwit-oss/tantivy/issues/1954) (@fulmicoton)
  - Improve fast field range query performance [#1864](https://github.com/quickwit-oss/tantivy/issues/1864) (@PSeitz)
- Make BM25 scoring more flexible [#1855](https://github.com/quickwit-oss/tantivy/issues/1855) (@alexcole)
- Switch fs2 to fs4 as it is now unmaintained and does not support illumos [#1944](https://github.com/quickwit-oss/tantivy/issues/1944) (@Toasterson)
- Made BooleanWeight and BoostWeight public [#1991](https://github.com/quickwit-oss/tantivy/issues/1991) (@fulmicoton)
- Make index compatible with virtual drives on Windows [#1843](https://github.com/quickwit-oss/tantivy/issues/1843) (@gyk)
- Add stop words for Hungarian language [#2069](https://github.com/quickwit-oss/tantivy/issues/2069) (@tnxbutno)
- Auto downgrade index record option, instead of vint error [#1857](https://github.com/quickwit-oss/tantivy/issues/1857) (@PSeitz)
- Enable range query on fast field for u64 compatible types [#1762](https://github.com/quickwit-oss/tantivy/issues/1762) (@PSeitz) [#1876]
- sstable
  - Isolating sstable and stacker in independent crates. [#1718](https://github.com/quickwit-oss/tantivy/issues/1718) (@fulmicoton)
  - New sstable format [#1943](https://github.com/quickwit-oss/tantivy/issues/1943)[#1953](https://github.com/quickwit-oss/tantivy/issues/1953) (@trinity-1686a)
  - Use DeltaReader directly to implement Dictionary::ord_to_term [#1928](https://github.com/quickwit-oss/tantivy/issues/1928) (@trinity-1686a)
  - Use DeltaReader directly to implement Dictionary::term_ord [#1925](https://github.com/quickwit-oss/tantivy/issues/1925) (@trinity-1686a)
- Add separate tokenizer manager for fast fields [#2019](https://github.com/quickwit-oss/tantivy/issues/2019) (@PSeitz)
- Make construction of LevenshteinAutomatonBuilder for FuzzyTermQuery instances lazy. [#1756](https://github.com/quickwit-oss/tantivy/issues/1756) (@adamreichold)
- Added support for madvise when opening an mmapped Index [#2036](https://github.com/quickwit-oss/tantivy/issues/2036) (@fulmicoton)
- Rename `DatePrecision` to `DateTimePrecision` [#2051](https://github.com/quickwit-oss/tantivy/issues/2051) (@guilload)
- Query Parser
  - Quotation mark can now be used for phrase queries. [#2050](https://github.com/quickwit-oss/tantivy/issues/2050) (@fulmicoton)
  - PhrasePrefixQuery is supported in the query parser via: `field:"phrase ter"*` [#2044](https://github.com/quickwit-oss/tantivy/issues/2044) (@adamreichold)
- Docs
  - Update examples for literate docs [#1880](https://github.com/quickwit-oss/tantivy/issues/1880) (@PSeitz)
  - Add ip field example [#1775](https://github.com/quickwit-oss/tantivy/issues/1775) (@PSeitz)
  - Fix doc store cache documentation [#1821](https://github.com/quickwit-oss/tantivy/issues/1821) (@PSeitz)
  - Fix BooleanQuery document [#1999](https://github.com/quickwit-oss/tantivy/issues/1999) (@RT_Enzyme)
  - Update comments in the faceted search example [#1737](https://github.com/quickwit-oss/tantivy/issues/1737) (@DawChihLiou)


Tantivy 0.19
================================
#### Bugfixes
- Fix missing fieldnorms for u64, i64, f64, bool, bytes and date [#1620](https://github.com/quickwit-oss/tantivy/pull/1620) (@PSeitz)
- Fix interpolation overflow in linear interpolation fastfield codec [#1480](https://github.com/quickwit-oss/tantivy/pull/1480) (@PSeitz @fulmicoton)

#### Features/Improvements
- Add support for `IN` in queryparser , e.g. `field: IN [val1 val2 val3]` [#1683](https://github.com/quickwit-oss/tantivy/pull/1683) (@trinity-1686a)
- Skip score calculation, when no scoring is required [#1646](https://github.com/quickwit-oss/tantivy/pull/1646) (@PSeitz)
- Limit fast fields to u32 (`get_val(u32)`) [#1644](https://github.com/quickwit-oss/tantivy/pull/1644) (@PSeitz)
- The `DateTime` type has been updated to hold timestamps with microseconds precision.
  `DateOptions` and `DatePrecision` have been added to configure Date fields. The precision is used to hint on fast values compression. Otherwise, seconds precision is used everywhere else (i.e terms, indexing) [#1396](https://github.com/quickwit-oss/tantivy/pull/1396) (@evanxg852000)
- Add IP address field type [#1553](https://github.com/quickwit-oss/tantivy/pull/1553) (@PSeitz)
- Add boolean field type [#1382](https://github.com/quickwit-oss/tantivy/pull/1382) (@boraarslan)
- Remove Searcher pool and make `Searcher` cloneable. (@PSeitz)
- Validate settings on create [#1570](https://github.com/quickwit-oss/tantivy/pull/1570) (@PSeitz)
- Detect and apply gcd on fastfield codecs [#1418](https://github.com/quickwit-oss/tantivy/pull/1418) (@PSeitz)
- Doc store
  - use separate thread to compress block store [#1389](https://github.com/quickwit-oss/tantivy/pull/1389) [#1510](https://github.com/quickwit-oss/tantivy/pull/1510) (@PSeitz @fulmicoton)
  - Expose doc store cache size [#1403](https://github.com/quickwit-oss/tantivy/pull/1403) (@PSeitz)
  - Enable compression levels for doc store [#1378](https://github.com/quickwit-oss/tantivy/pull/1378) (@PSeitz)
  - Make block size configurable [#1374](https://github.com/quickwit-oss/tantivy/pull/1374) (@kryesh)
- Make `tantivy::TantivyError` cloneable [#1402](https://github.com/quickwit-oss/tantivy/pull/1402) (@PSeitz)
- Add support for phrase slop in query language [#1393](https://github.com/quickwit-oss/tantivy/pull/1393) (@saroh)
- Aggregation
  - Add aggregation support for date type [#1693](https://github.com/quickwit-oss/tantivy/pull/1693)(@PSeitz)
  - Add support for keyed parameter in range and histogram aggregations [#1424](https://github.com/quickwit-oss/tantivy/pull/1424) (@k-yomo)
  - Add aggregation bucket limit [#1363](https://github.com/quickwit-oss/tantivy/pull/1363) (@PSeitz)
- Faster indexing
  - [#1610](https://github.com/quickwit-oss/tantivy/pull/1610) (@PSeitz)
  - [#1594](https://github.com/quickwit-oss/tantivy/pull/1594) (@PSeitz)
  - [#1582](https://github.com/quickwit-oss/tantivy/pull/1582) (@PSeitz)
  - [#1611](https://github.com/quickwit-oss/tantivy/pull/1611) (@PSeitz)
  - Added a pre-configured stop word filter for various language [#1666](https://github.com/quickwit-oss/tantivy/pull/1666) (@adamreichold)

Tantivy 0.18
================================

- For date values `chrono` has been replaced with `time` (@uklotzde) #1304 :
  - The `time` crate is re-exported as `tantivy::time` instead of `tantivy::chrono`.
  - The type alias `tantivy::DateTime` has been removed.
  - `Value::Date` wraps `time::PrimitiveDateTime` without time zone information.
  - Internally date/time values are stored as seconds since UNIX epoch in UTC.
  - Converting a `time::OffsetDateTime` to `Value::Date` implicitly converts the value into UTC.
    If this is not desired do the time zone conversion yourself and use `time::PrimitiveDateTime`
    directly instead.
- Add [histogram](https://github.com/quickwit-oss/tantivy/pull/1306) aggregation (@PSeitz)
- Add support for fastfield on text fields (@PSeitz)
- Add terms aggregation (@PSeitz)
- Add support for zstd compression (@kryesh)

Tantivy 0.18.1
================================
- Hotfix: positions computation.  #1629 (@fmassot, @fulmicoton, @PSeitz)

Tantivy 0.17
================================

- LogMergePolicy now triggers merges if the ratio of deleted documents reaches a threshold (@shikhar @fulmicoton) [#115](https://github.com/quickwit-oss/tantivy/issues/115)
- Adds a searcher Warmer API (@shikhar @fulmicoton)
- Change to non-strict schema. Ignore fields in data which are not defined in schema. Previously this returned an error. #1211
- Facets are necessarily indexed. Existing index with indexed facets should work out of the box. Index without facets that are marked with index: false should be broken (but they were already broken in a sense). (@fulmicoton) #1195 .
- Bugfix that could in theory impact durability in theory on some filesystems [#1224](https://github.com/quickwit-oss/tantivy/issues/1224)
- Schema now offers not indexing fieldnorms (@lpouget) [#922](https://github.com/quickwit-oss/tantivy/issues/922)
- Reduce the number of fsync calls [#1225](https://github.com/quickwit-oss/tantivy/issues/1225)
- Fix opening bytes index with dynamic codec (@PSeitz) [#1278](https://github.com/quickwit-oss/tantivy/issues/1278)
- Added an aggregation collector for range, average and stats compatible with Elasticsearch. (@PSeitz)
- Added a JSON schema type @fulmicoton [#1251](https://github.com/quickwit-oss/tantivy/issues/1251)
- Added support for slop in phrase queries @halvorboe [#1068](https://github.com/quickwit-oss/tantivy/issues/1068)

Tantivy 0.16.2
================================

- Bugfix in FuzzyTermQuery. (transposition_cost_one was not doing anything)

Tantivy 0.16.1
========================

- Major Bugfix on multivalued fastfield.  #1151
- Demux operation (@PSeitz)

Tantivy 0.16.0
=========================

- Bugfix in the filesum check. (@evanxg852000) #1127
- Bugfix in positions when the index is sorted by a field. (@appaquet) #1125

Tantivy 0.15.3
=========================

- Major bugfix. Deleting documents was broken when the index was sorted by a field. (@appaquet, @fulmicoton) #1101

Tantivy 0.15.2
========================

- Major bugfix. DocStore still panics when a deleted doc is at the beginning of a block. (@appaquet) #1088

Tantivy 0.15.1
=========================

- Major bugfix. DocStore panics when first block is deleted. (@appaquet) #1077

Tantivy 0.15.0
=========================

- API Changes. Using Range instead of (start, end) in the API and internals (`FileSlice`, `OwnedBytes`, `Snippets`, ...)
  This change is breaking but migration is trivial.
- Added an Histogram collector. (@fulmicoton) #994
- Added support for Option<TCollector>.  (@fulmicoton)
- DocAddress is now a struct (@scampi) #987
- Bugfix consistent tie break handling in facet's topk (@hardikpnsp) #357
- Date field support for range queries (@rihardsk) #516
- Added lz4-flex as the default compression scheme in tantivy (@PSeitz) #1009
- Renamed a lot of symbols to avoid all uppercasing on acronyms, as per new clippy recommendation. For instance, RAMDirectory -> RamDirectory. (@fulmicoton)
- Simplified positions index format (@fulmicoton) #1022
- Moved bitpacking to bitpacker subcrate and add BlockedBitpacker, which bitpacks blocks of 128 elements (@PSeitz) #1030
- Added support for more-like-this query in tantivy (@evanxg852000) #1011
- Added support for sorting an index, e.g presorting documents in an index by a timestamp field. This can heavily improve performance for certain scenarios, by utilizing the sorted data (Top-n optimizations)(@PSeitz). #1026
- Add iterator over documents in doc store (@PSeitz). #1044
- Fix log merge policy (@PSeitz). #1043
- Add detection to avoid small doc store blocks on merge (@PSeitz). #1054
- Make doc store compression dynamic (@PSeitz). #1060
- Switch to json for footer version handling (@PSeitz). #1060
- Updated TermMerger implementation to rely on the union feature of the FST (@scampi) #469
- Add boolean marking whether position is required in the query_terms API call (@fulmicoton). #1070

Tantivy 0.14.0
=========================

- Remove dependency to atomicwrites #833 .Implemented by @fulmicoton upon suggestion and research from @asafigan).
- Migrated tantivy error from the now deprecated `failure` crate to `thiserror` #760. (@hirevo)
- API Change. Accessing the typed value off a `Schema::Value` now returns an Option instead of panicking if the type does not match.
- Large API Change in the Directory API. Tantivy used to assume that all files could be somehow memory mapped. After this change, Directory return a `FileSlice` that can be reduced and eventually read into an `OwnedBytes` object. Long and blocking io operation are still required by they do not span over the entire file.
- Added support for Brotli compression in the DocStore. (@ppodolsky)
- Added helper for building intersections and unions in BooleanQuery (@guilload)
- Bugfix in `Query::explain`
- Removed dependency on `notify` #924. Replaced with `FileWatcher` struct that polls meta file every 500ms in background thread. (@halvorboe @guilload)
- Added `FilterCollector`, which wraps another collector and filters docs using a predicate over a fast field (@barrotsteindev)
- Simplified the encoding of the skip reader struct. BlockWAND max tf is now encoded over a single byte. (@fulmicoton)
- `FilterCollector` now supports all Fast Field value types (@barrotsteindev)
- FastField are not all loaded when opening the segment reader. (@fulmicoton)
- Added an API to merge segments, see `tantivy::merge_segments` #1005. (@evanxg852000)

This version breaks compatibility and requires users to reindex everything.

Tantivy 0.13.2
===================

Bugfix. Acquiring a facet reader on a segment that does not contain any
doc with this facet returns `None`. (#896)

Tantivy 0.13.1
===================

Made `Query` and `Collector` `Send + Sync`.
Updated misc dependency versions.

Tantivy 0.13.0
======================

Tantivy 0.13 introduce a change in the index format that will require
you to reindex your index (BlockWAND information are added in the skiplist).
The index size increase is minor as this information is only added for
full blocks.
If you have a massive index for which reindexing is not an option, please contact me
so that we can discuss possible solutions.

- Bugfix in `FuzzyTermQuery` not matching terms by prefix when it should (@Peachball)
- Relaxed constraints on the custom/tweak score functions. At the segment level, they can be mut, and they are not required to be Sync + Send.
- `MMapDirectory::open` does not return a `Result` anymore.
- Change in the DocSet and Scorer API. (@fulmicoton).
A freshly created DocSet point directly to their first doc. A sentinel value called TERMINATED marks the end of a DocSet.
`.advance()` returns the new DocId. `Scorer::skip(target)` has been replaced by `Scorer::seek(target)` and returns the resulting DocId.
As a result, iterating through DocSet now looks as follows

```rust
let mut doc = docset.doc();
while doc != TERMINATED {
   // ...
   doc = docset.advance();
}
```

The change made it possible to greatly simplify a lot of the docset's code.

- Misc internal optimization and introduction of the `Scorer::for_each_pruning` function. (@fulmicoton)
- Added an offset option to the Top(.*)Collectors. (@robyoung)
- Added Block WAND. Performance on TOP-K on term-unions should be greatly increased. (@fulmicoton, and special thanks
to the PISA team for answering all my questions!)

Tantivy 0.12.0
======================

- Removing static dispatch in tokenizers for simplicity. (#762)
- Added backward iteration for `TermDictionary` stream. (@halvorboe)
- Fixed a performance issue when searching for the posting lists of a missing term (@audunhalland)
- Added a configurable maximum number of docs (10M by default) for a segment to be considered for merge (@hntd187, landed by @halvorboe #713)
- Important Bugfix #777, causing tantivy to retain memory mapping. (diagnosed by @poljar)
- Added support for field boosting. (#547, @fulmicoton)

## How to update?

Crates relying on custom tokenizer, or registering tokenizer in the manager will require some
minor changes. Check <https://github.com/quickwit-oss/tantivy/blob/main/examples/custom_tokenizer.rs>
to check for some code sample.

Tantivy 0.11.3
=======================

- Fixed DateTime as a fast field (#735)

Tantivy 0.11.2
=======================

- The future returned by `IndexWriter::merge` does not borrow `self` mutably anymore (#732)
- Exposing a constructor for `WatchHandle` (#731)

Tantivy 0.11.1
=====================

- Bug fix #729

Tantivy 0.11.0
=====================

- Added f64 field. Internally reuse u64 code the same way i64 does (@fdb-hiroshima)
- Various bugfixes in the query parser.
  - Better handling of hyphens in query parser. (#609)
  - Better handling of whitespaces.
- Closes #498 - add support for Elastic-style unbounded range queries for alphanumeric types eg. "title:>hello", "weight:>=70.5", "height:<200" (@petr-tik)
- API change around `Box<BoxableTokenizer>`. See detail in #629
- Avoid rebuilding Regex automaton whenever a regex query is reused. #639 (@brainlock)
- Add footer with some metadata to index files. #605 (@fdb-hiroshima)
- Add a method to check the compatibility of the footer in the index with the running version of tantivy (@petr-tik)
- TopDocs collector: ensure stable sorting on equal score. #671 (@brainlock)
- Added handling of pre-tokenized text fields (#642), which will enable users to
  load tokens created outside tantivy. See usage in examples/pre_tokenized_text. (@kkoziara)
- Fix crash when committing multiple times with deleted documents. #681 (@brainlock)

## How to update?

- The index format is changed. You are required to reindex your data to use tantivy 0.11.
- `Box<dyn BoxableTokenizer>` has been replaced by a `BoxedTokenizer` struct.
- Regex are now compiled when the `RegexQuery` instance is built. As a result, it can now return
an error and handling the `Result` is required.
- `tantivy::version()` now returns a `Version` object. This object implements `ToString()`

Tantivy 0.10.2
=====================

- Closes #656. Solving memory leak.

Tantivy 0.10.1
=====================

- Closes #544.  A few users experienced problems with the directory watching system.
Avoid watching the mmap directory until someone effectively creates a reader that uses
this functionality.

Tantivy 0.10.0
=====================

*Tantivy 0.10.0 index format is compatible with the index format in 0.9.0.*

- Added an API to easily tweak or entirely replace the
 default score. See `TopDocs::tweak_score`and `TopScore::custom_score` (@fulmicoton)
- Added an ASCII folding filter (@drusellers)
- Bugfix in `query.count` in presence of deletes (@fulmicoton)
- Added `.explain(...)` in `Query` and `Weight` to (@fulmicoton)
- Added an efficient way to `delete_all_documents` in `IndexWriter` (@petr-tik).
  All segments are simply removed.

Minor
---------

- Switched to Rust 2018 (@uvd)
- Small simplification of the code.
Calling .freq() or .doc() when .advance() has never been called
on segment postings should panic from now on.
- Tokens exceeding `u16::max_value() - 4` chars are discarded silently instead of panicking.
- Fast fields are now preloaded when the `SegmentReader` is created.
- `IndexMeta` is now public.  (@hntd187)
- `IndexWriter` `add_document`, `delete_term`. `IndexWriter` is `Sync`, making it possible to use it with a `Arc<RwLock<IndexWriter>>`. `add_document` and `delete_term` can
only require a read lock. (@fulmicoton)
- Introducing `Opstamp` as an expressive type alias for `u64`. (@petr-tik)
- Stamper now relies on `AtomicU64` on all platforms (@petr-tik)
- Bugfix - Files get deleted slightly earlier
- Compilation resources improved (@fdb-hiroshima)

## How to update?

Your program should be usable as is.

### Fast fields

Fast fields used to be accessed directly from the `SegmentReader`.
The API changed, you are now required to acquire your fast field reader via the
`segment_reader.fast_fields()`, and use one of the typed method:

- `.u64()`, `.i64()` if your field is single-valued ;
- `.u64s()`, `.i64s()` if your field is multi-valued ;
- `.bytes()` if your field is bytes fast field.

Tantivy 0.9.0
=====================

*0.9.0 index format is not compatible with the
previous index format.*

- MAJOR BUGFIX :
  Some `Mmap` objects were being leaked, and would never get released. (@fulmicoton)
- Removed most unsafe (@fulmicoton)
- Indexer memory footprint improved. (VInt comp, inlining the first block. (@fulmicoton)
- Stemming in other language possible (@pentlander)
- Segments with no docs are deleted earlier (@barrotsteindev)
- Added grouped add and delete operations.
  They are guaranteed to happen together (i.e. they cannot be split by a commit).
  In addition, adds are guaranteed to happen on the same segment. (@elbow-jason)
- Removed `INT_STORED` and `INT_INDEXED`. It is now possible to use `STORED` and `INDEXED`
  for int fields. (@fulmicoton)
- Added DateTime field (@barrotsteindev)
- Added IndexReader. By default, index is reloaded automatically upon new commits (@fulmicoton)
- SIMD linear search within blocks (@fulmicoton)

## How to update ?

tantivy 0.9 brought some API breaking change.
To update from tantivy 0.8, you will need to go through the following steps.

- `schema::INT_INDEXED` and `schema::INT_STORED`  should be replaced by `schema::INDEXED` and `schema::INT_STORED`.
- The index now does not hold the pool of searcher anymore. You are required to create an intermediary object called
`IndexReader` for this.

    ```rust
    // create the reader. You typically need to create 1 reader for the entire
    // lifetime of you program.
    let reader = index.reader()?;

    // Acquire a searcher (previously `index.searcher()`) is now written:
    let searcher = reader.searcher();

    // With the default setting of the reader, you are not required to
    // call `index.load_searchers()` anymore.
    //
    // The IndexReader will pick up that change automatically, regardless
    // of whether the update was done in a different process or not.
    // If this behavior is not wanted, you can create your reader with
    // the `ReloadPolicy::Manual`, and manually decide when to reload the index
    // by calling `reader.reload()?`.

    ```

Tantivy 0.8.2
=====================

Fixing build for x86_64 platforms. (#496)
No need to update from 0.8.1 if tantivy
is building on your platform.

Tantivy 0.8.1
=====================

Hotfix of #476.

Merge was reflecting deletes before commit was passed.
Thanks @barrotsteindev  for reporting the bug.

Tantivy 0.8.0
=====================

*No change in the index format*

- API Breaking change in the collector API. (@jwolfe, @fulmicoton)
- Multithreaded search (@jwolfe, @fulmicoton)

Tantivy 0.7.1
=====================

*No change in the index format*

- Bugfix: NGramTokenizer panics on non ascii chars
- Added a space usage API

Tantivy 0.7
=====================

- Skip data for doc ids and positions (@fulmicoton),
  greatly improving performance
- Tantivy error now rely on the failure crate (@drusellers)
- Added support for `AND`, `OR`, `NOT` syntax in addition to the `+`,`-` syntax
- Added a snippet generator with highlight (@vigneshsarma, @fulmicoton)
- Added a `TopFieldCollector` (@pentlander)

Tantivy 0.6.1
=========================

- Bugfix #324. GC removing was removing file that were still in useful
- Added support for parsing AllQuery and RangeQuery via QueryParser
  - AllQuery: `*`
  - RangeQuery:
    - Inclusive `field:[startIncl to endIncl]`
    - Exclusive `field:{startExcl to endExcl}`
    - Mixed `field:[startIncl to endExcl}` and vice versa
    - Unbounded `field:[start to *]`, `field:[* to end]`

Tantivy 0.6
==========================

Special thanks to @drusellers and @jason-wolfe for their contributions
to this release!

- Removed C code. Tantivy is now pure Rust. (@fulmicoton)
- BM25 (@fulmicoton)
- Approximate field norms encoded over 1 byte. (@fulmicoton)
- Compiles on stable rust (@fulmicoton)
- Add &[u8] fastfield for associating arbitrary bytes to each document (@jason-wolfe) (#270)
  - Completely uncompressed
  - Internally: One u64 fast field for indexes, one fast field for the bytes themselves.
- Add NGram token support (@drusellers)
- Add Stopword Filter support (@drusellers)
- Add a FuzzyTermQuery (@drusellers)
- Add a RegexQuery (@drusellers)
- Various performance improvements (@fulmicoton)_

Tantivy 0.5.2
===========================

- bugfix #274
- bugfix #280
- bugfix #289

Tantivy 0.5.1
==========================

- bugfix #254 : tantivy failed if no documents in a segment contained a specific field.

Tantivy 0.5
==========================

- Faceting
- RangeQuery
- Configurable tokenization pipeline
- Bugfix in PhraseQuery
- Various query optimisation
- Allowing very large indexes
  - 64 bits file address
  - Smarter encoding of the `TermInfo` objects

Tantivy 0.4.3
==========================

- Bugfix race condition when deleting files. (#198)

Tantivy 0.4.2
==========================

- Prevent usage of AVX2 instructions (#201)

Tantivy 0.4.1
==========================

- Bugfix for non-indexed fields. (#199)

Tantivy 0.4.0
==========================

- Raise the limit of number of fields (previously 256 fields) (@fulmicoton)
- Removed u32 fields. They are replaced by u64 and i64 fields (#65) (@fulmicoton)
- Optimized skip in SegmentPostings (#130) (@lnicola)
- Replacing rustc_serialize by serde. Kudos to  benchmark@KodrAus and @lnicola
- Using error-chain (@KodrAus)
- QueryParser: (@fulmicoton)
  - Explicit error returned when searched for a term that is not indexed
  - Searching for a int term via the query parser was broken `(age:1)`
  - Searching for a non-indexed field returns an explicit Error
  - Phrase query for non-tokenized field are not tokenized by the query parser.
- Faster/Better indexing (@fulmicoton)
  - using murmurhash2
  - faster merging
  - more memory efficient fast field writer (@lnicola )
  - better handling of collisions
  - lesser memory usage
- Added API, most notably to iterate over ranges of terms (@fulmicoton)
- Bugfix that was preventing to unmap segment files, on index drop (@fulmicoton)
- Made the doc! macro public (@fulmicoton)
- Added an alternative implementation of the streaming dictionary (@fulmicoton)

Tantivy 0.3.1
==========================

- Expose a method to trigger files garbage collection

Tantivy 0.3
==========================

Special thanks to @Kodraus @lnicola @Ameobea @manuel-woelker @celaus
for their contribution to this release.

Thanks also to everyone in tantivy gitter chat
for their advise and company :)

<https://gitter.im/tantivy-search/tantivy>

Warning:

Tantivy 0.3 is NOT backward compatible with tantivy 0.2
code and index format.
You should not expect backward compatibility before
tantivy 1.0.

New Features
------------

- Delete. You can now delete documents from an index.
- Support for windows (Thanks to @lnicola)

Various Bugfixes & small improvements
----------------------------------------

- Added CI for Windows (<https://ci.appveyor.com/project/fulmicoton/tantivy>)
Thanks to @KodrAus ! (#108)
- Various dependy version update (Thanks to @Ameobea) #76
- Fixed several race conditions in `Index.wait_merge_threads`
- Fixed #72. Mmap were never released.
- Fixed #80. Fast field used to take an amplitude of 32 bits after a merge. (Ouch!)
- Fixed #92. u32 are now encoded using big endian in the fst
  in order to make there enumeration consistent with
  the natural ordering.
- Building binary targets for tantivy-cli (Thanks to @KodrAus)
- Misc invisible bug fixes, and code cleanup.
- Use
