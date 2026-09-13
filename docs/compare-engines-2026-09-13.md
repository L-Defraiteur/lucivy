# lucivy against Elasticsearch and tantivy — one corpus, one truth

Corpus: 101 373 files, 899 MB of text, linux v7.2 at commit 8d3ae59288f1 (text files of 100 KB at most, no binaries, the same selection for every engine). The truth of every row is a byte-by-byte scan of the files by lucivy's ground-truth harness; a lucivy count is only reported `OK` when its documents **and** its byte spans match that scan. A count in bold equals the truth.

## 1. Index size and indexing time

| engine | how it answers a substring | index | × text | indexing |
|---|---|---|---|---|
| Elasticsearch 8.19, standard analyzer | it does not (whole words) | 575 MB | ×0.6 | reused |
| Elasticsearch 8.19, trigram analyzer + `wildcard` field | trigram phrases; regex on the wildcard field | 3 050 MB | ×3.4 | reused |
| tantivy 0.25, default tokenizer | it does not (whole words) | 653 MB | ×0.7 | 1 s |
| tantivy 0.25, `NgramTokenizer` (trigrams) | trigram phrases (positions all 0: candidates only) | 723 MB | ×0.8 | 5 s |
| lucivy 4.2, a dictionary per segment (`sfx_version` 3) | suffix FST, exact spans | 6 886 MB | ×7.7 | 51 s |
| lucivy 4.2, shared dictionary per shard | suffix FST, exact spans | 5 064 MB | ×5.6 | 47 s |
| lucivy 4.2, shared dictionary + `derived_in_ram` | suffix FST, exact spans | 3 408 MB | ×3.8 | 46 s |
| lucivy 4.2, shared dictionary + `positions: false` | suffix FST, exact spans | 2 491 MB | ×2.8 | 47 s |

## 2. The nine verified queries

| query | mode | truth (scan) | lucivy | spans | lucivy | lucivy, `positions: false` | Elasticsearch | tantivy |
|---|---|---|---|---|---|---|---|---|
| `mutex_lock` | substring | 5 202 | **5 202** OK | 21 070 | 13 ms | 28 ms | **5 202** · 1 ms (+403 ms with highlights on 200) | 5 229 · 109 ms |
| `mutex_lock` | separators relaxed | 5 862 | **5 862** OK | 23 067 | 12 ms | 38 ms | — | — |
| `spin_lock` | substring | 6 527 | **6 527** OK | 34 436 | 13 ms | 35 ms | **6 527** · 1 ms (+277 ms with highlights on 200) | 6 563 · 117 ms |
| `sched` | whole word | 5 246 | **5 246** OK | 27 341 | 21 ms | 58 ms | 1 724 · 1 ms (+116 ms with highlights on 200) | 5 260 · 0 ms |
| `sched` | substring | 9 214 | **9 214** OK | 52 228 | 12 ms | 43 ms | 9 181 · 0 ms (+201 ms with highlights on 200) | 9 238 · 148 ms |
| `printk` | start of token | 4 446 | **4 446** OK | 24 702 | 15 ms | 38 ms | 3 164 · 1 ms (+132 ms with highlights on 200) | 4 399 · 0 ms |
| `schdule` | fuzzy, 1 edit | 5 148 | **5 148** OK | 18 549 | 51 ms | 198 ms | 1 531 · 0 ms (+170 ms with highlights on 200) | 3 721 · 5 ms |
| `regsiter` | fuzzy, 2 edits | 34 833 | **34 833** OK | 265 247 | 842 ms | 407 ms | 21 165 · 1 ms (+461 ms with highlights on 200) | 29 438 · 16 ms |
| `spin_lock_[a-z]+` | regex | 5 471 | **5 471** OK | 24 156 | 236 ms | 22 ms | **5 471** · 4 ms (+1167 ms with highlights on 200) | 0 · 0 ms |

lucivy's two times are the search alone, documents **and** every span: the shared dictionary, then the same index built without positions (`positions: false`), which verifies each match on the stored text. Elasticsearch's first number is its own `took` for the documents; the second, where it could be measured, is what `highlight` adds to mark the spans of the top 200 — the only comparable figure, since a span is what lucivy returns with the answer. tantivy's is the count, or for substrings the whole verified path (see §3). Whole-word and prefix counts depend on each engine's definition of a word: lucivy's harness counts `sched` bounded by separators on both sides; the standard analyzer keeps `sched_clock` as one term and splits on `/`, so its whole-word and prefix rows are close but not equal. Elasticsearch runs the substring rows on its trigram index and the whole-word, prefix and fuzzy rows on its standard one; tantivy likewise. A fuzzy row that is not bold is not a miscount: their fuzziness compares whole terms, lucivy's a substring that may cross a separator — the questions differ, and the row shows by how much.

## 3. Where the questions differ

| what is asked | truth (scan) | lucivy | Elasticsearch | tantivy |
|---|---|---|---|---|
| `spin_lock`, separators strict | 6 527 | **6 527** OK, 34 436 spans, 13 ms | **6 527** (spin_lock, separators strict, 0 ms +166 ms with highlights on 200) | 6 563 (spin_lock (substring), 117 ms)<br>6 563 (spin_lock (trigrams, verified, strict), 117 ms) |
| `spin_lock`, separators relaxed — also `spin lock`, `spin-lock`, `spinlock` | 9 545 | **9 545** OK, 54 680 spans, 44 ms | 6 524 (spin_lock, separators relaxed (spin_lock, spin lock, spin-lock, spinlock), 0 ms +138 ms with highlights on 200)<br>170 ("spin lock" as a phrase, standard analyzer, 0 ms +171 ms with highlights on 170) | 6 608 (spinlock (trigrams, verified; must find spin_lock too), 114 ms)<br>6 594 ("spin lock" (phrase, default tokenizer), 1 ms) |
| `spinlokc`, two edits, across the token boundary | 10 117 | **10 117** OK, 56 836 spans, 169 ms | 3 534 (spinlokc, two edits, across the token boundary, 0 ms +463 ms with highlights on 200) | 6 585 (spinlokc (fuzzy, 2 edits, across the boundary), 17 ms) |
| `spin_lock_[a-z]+`, a regex | 5 471 | **5 471** OK, 24 156 spans, 236 ms | **5 471** (spin_lock_[a-z]+ (regex, wildcard field, case folded), 3 ms +846 ms with highlights on 200) | 0 (spin_lock_[a-z]+ (regex, terms), 0 ms) |
| `ude`, three characters | 74 500 | **74 500** OK, 478 423 spans, 106 ms | 68 561 (ude (three characters), 0 ms +92 ms with highlights on 200) | 74 679 (ude (three characters), 0 ms) |
| `de`, two characters | 100 166 | **100 166** OK, 7 929 772 spans, 613 ms | 0 (de (two characters), 1 ms +0 ms with highlights on 0) | 0 (de (two characters), 0 ms) |
| `retur -ENOMEM`, a fuzzy phrase (one edit: a letter missing) | 14 377 | **14 377** OK, 31 937 spans, 41 ms | 14 374 (retur -ENOMEM (fuzzy phrase: span_near of a fuzzy span and a term), 0 ms +151 ms with highlights on 200) | — |

Read across a row: the same question, and what each engine can make of it. lucivy's time is the documents **and** every span; Elasticsearch's first number is its `took` for the documents alone, the second what `highlight` adds to mark the spans of the top 200 — without it the two columns would not be answering the same question (an Elasticsearch time here may also be a cache hit: the same query already ran in §2). Elasticsearch keeps separators in its trigram field, so its strict rows land exactly; tantivy's default tokenizer cannot keep them (the separator never enters the index), and its n-gram tokenizer emits every position as 0, so its substring rows are an AND of trigrams verified by reading each candidate's stored text — the application's work, timed here as such. Both engines' fuzziness stops at their token boundary. An n-gram index has nothing to look up below three characters. The fuzzy phrase is the case Elasticsearch handles well, with `span_near`.

## 4. The price of knowing where

| engine | documents | spans reported | in how many documents | time |
|---|---|---|---|---|
| lucivy (every document, every span, verified) | 5 202 | 21 070 | all 5 202 | 13 ms |
| Elasticsearch, `highlight` on the top 200 | 5 202 | 2 485 (as marked by the engine) | 200 | 187 ms + 0.3 ms to parse 2.9 MB of markup |
| tantivy, AND of trigrams verified on the stored text, occurrences in the first 200 | 5 229 | 730 | 200 | 96 ms (the whole path) |

`mutex_lock`, separators strict. lucivy's spans come out of the index with the documents. Elasticsearch re-reads and re-analyses each hit's stored text (`highlight`), priced on the top 200. tantivy's trigram index has no usable positions (its n-gram tokenizer emits 0 for every token, so a trigram phrase matches nothing): the honest path is an AND of trigrams, then reading every candidate's stored text to verify the substring and count its occurrences — the time shown is that whole path, occurrences counted in the first 200 verified documents.

## How this was produced

`benches/compare_engines.sh <corpus>`: lucivy's ground-truth harness (`lucivy_core/tests/test_sfx_v3_ground_truth.rs`, `v3_ground_truth_demo`, then the same with `V3_QUERIES` for section 3), `lucivy_core/benches/compare_tantivy.rs` (tantivy 0.25 from crates.io, not the fork) and `benches/compare_elasticsearch.py` (Elasticsearch 8.19 in a container, configured at its best: trigram analyzer, `wildcard` field). Logs and JSON next to this file.
