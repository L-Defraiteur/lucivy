# lucivy against Elasticsearch and tantivy — one corpus, one truth

Corpus: 101 373 files, 899 MB of text, linux v7.2 at commit 8d3ae59288f1 (text files of 100 KB at most, no binaries, the same selection for every engine). The truth of every row is a byte-by-byte scan of the files by lucivy's ground-truth harness; a lucivy count is only reported `OK` when its documents **and** its byte spans match that scan. A count in bold equals the truth.

## 1. Index size and indexing time

| engine | how it answers a substring | index | × text | indexing |
|---|---|---|---|---|
| Elasticsearch 8.19, standard analyzer | it does not (whole words) | 722 MB | ×0.8 | 28 s |
| Elasticsearch 8.19, trigram analyzer + `wildcard` field | trigram phrases; regex on the wildcard field | 3 050 MB | ×3.4 | 118 s |
| tantivy 0.25, default tokenizer | it does not (whole words) | 657 MB | ×0.7 | 1 s |
| tantivy 0.25, `NgramTokenizer` (trigrams) | trigram phrases (positions all 0: candidates only) | 735 MB | ×0.8 | 5 s |
| lucivy 4.0, a dictionary per segment (`sfx_version` 3) | suffix FST, exact spans | 6 821 MB | ×7.6 | 60 s |
| lucivy 4.0, shared dictionary per shard | suffix FST, exact spans | 5 044 MB | ×5.6 | 112 s |
| lucivy 4.0, shared dictionary + `derived_in_ram` | suffix FST, exact spans | 3 392 MB | ×3.8 | 108 s |
| lucivy 4.1, shared dictionary + `positions: false` | suffix FST, exact spans | 2 478 MB | ×2.8 | 94 s |

## 2. The nine verified queries

| query | mode | truth (scan) | lucivy | spans | lucivy | Elasticsearch | tantivy |
|---|---|---|---|---|---|---|---|
| `mutex_lock` | substring | 5 202 | **5 202** OK | 21 070 | 13 ms | **5 202** · 5 ms | 5 229 · 108 ms |
| `mutex_lock` | separators relaxed | 5 862 | **5 862** OK | 23 067 | 12 ms | — | — |
| `spin_lock` | substring | 6 527 | **6 527** OK | 34 436 | 12 ms | **6 527** · 4 ms | 6 563 · 118 ms |
| `sched` | whole word | 5 246 | **5 246** OK | 27 341 | 20 ms | 1 724 · 0 ms | 5 260 · 0 ms |
| `sched` | substring | 9 214 | **9 214** OK | 52 228 | 12 ms | 9 181 · 2 ms | 9 238 · 150 ms |
| `printk` | start of token | 4 446 | **4 446** OK | 24 702 | 14 ms | 3 164 · 4 ms | 4 399 · 0 ms |
| `schdule` | fuzzy, 1 edit | 5 148 | **5 148** OK | 18 549 | 51 ms | 1 531 · 5 ms | 3 721 · 4 ms |
| `regsiter` | fuzzy, 2 edits | 34 833 | **34 833** OK | 265 247 | 860 ms | 21 165 · 17 ms | 29 438 · 16 ms |
| `spin_lock_[a-z]+` | regex | 5 471 | **5 471** OK | 24 156 | 237 ms | **5 471** · 452 ms | 0 · 0 ms |

lucivy's time is the search alone (documents and every span); Elasticsearch's is its own `took`, first run of each query; tantivy's is the count, or for substrings the whole verified path (see §3). Whole-word and prefix counts depend on each engine's definition of a word: lucivy's harness counts `sched` bounded by separators on both sides; the standard analyzer keeps `sched_clock` as one term and splits on `/`, so its whole-word and prefix rows are close but not equal. Elasticsearch runs the substring rows on its trigram index and the whole-word, prefix and fuzzy rows on its standard one; tantivy likewise. A fuzzy row that is not bold is not a miscount: their fuzziness compares whole terms, lucivy's a substring that may cross a separator — the questions differ, and the row shows by how much.

## 3. Where the questions differ

| what is asked | truth (scan) | lucivy | Elasticsearch | tantivy |
|---|---|---|---|---|
| `spin_lock`, separators strict | 6 527 | **6 527** OK, 34 436 spans, 12 ms | **6 527** (spin_lock, separators strict, 1 ms) | 6 563 (spin_lock (substring), 118 ms)<br>6 563 (spin_lock (trigrams, verified, strict), 119 ms) |
| `spin_lock`, separators relaxed — also `spin lock`, `spin-lock`, `spinlock` | 9 545 | **9 545** OK, 54 680 spans, 41 ms | 6 524 (spin_lock, separators relaxed (spin_lock, spin lock, spin-lock, spinlock), 4 ms)<br>170 ("spin lock" as a phrase, standard analyzer, 1 ms) | 6 608 (spinlock (trigrams, verified; must find spin_lock too), 114 ms)<br>6 594 ("spin lock" (phrase, default tokenizer), 1 ms) |
| `spinlokc`, two edits, across the token boundary | 10 117 | **10 117** OK, 56 836 spans, 171 ms | 3 534 (spinlokc, two edits, across the token boundary, 14 ms) | 6 585 (spinlokc (fuzzy, 2 edits, across the boundary), 17 ms) |
| `spin_lock_[a-z]+`, a regex | 5 471 | **5 471** OK, 24 156 spans, 237 ms | **5 471** (spin_lock_[a-z]+ (regex, wildcard field, case folded), 1 ms) | 0 (spin_lock_[a-z]+ (regex, terms), 0 ms) |
| `ude`, three characters | 74 500 | **74 500** OK, 478 423 spans, 106 ms | 68 561 (ude (three characters), 0 ms) | 74 679 (ude (three characters), 0 ms) |
| `de`, two characters | 100 166 | **100 166** OK, 7 929 772 spans, 587 ms | 0 (de (two characters), 0 ms) | 0 (de (two characters), 0 ms) |
| `retur -ENOMEM`, a fuzzy phrase (one edit: a letter missing) | 14 377 | **14 377** OK, 31 937 spans, 40 ms | 14 374 (retur -ENOMEM (fuzzy phrase: span_near of a fuzzy span and a term), 9 ms) | — |

Read across a row: the same question, what each engine can make of it (an Elasticsearch time here may be a cache hit: the same query already ran in §2). (its trigrams carry them); tantivy's default tokenizer cannot keep them (the separator never enters the index), and its n-gram tokenizer emits every position as 0, so its substring rows are an AND of trigrams verified by reading each candidate's stored text — the application's work, timed here as such. Both engines' fuzziness stops at their token boundary. An n-gram index has nothing to look up below three characters. The fuzzy phrase is the case Elasticsearch handles well, with `span_near`.

## 4. The price of knowing where

| engine | documents | spans reported | in how many documents | time |
|---|---|---|---|---|
| lucivy (every document, every span, verified) | 5 202 | 21 070 | all 5 202 | 13 ms |
| Elasticsearch, `highlight` on the top 200 | 5 202 | 2 485 (as marked by the engine) | 200 | 108 ms + 0.3 ms to parse 2.9 MB of markup |
| tantivy, AND of trigrams verified on the stored text, occurrences in the first 200 | 5 229 | 825 | 200 | 96 ms (the whole path) |

`mutex_lock`, separators strict. lucivy's spans come out of the index with the documents. Elasticsearch re-reads and re-analyses each hit's stored text (`highlight`), priced on the top 200. tantivy's trigram index has no usable positions (its n-gram tokenizer emits 0 for every token, so a trigram phrase matches nothing): the honest path is an AND of trigrams, then reading every candidate's stored text to verify the substring and count its occurrences — the time shown is that whole path, occurrences counted in the first 200 verified documents.

## How this was produced

`benches/compare_engines.sh <corpus>`: lucivy's ground-truth harness (`lucivy_core/tests/test_sfx_v3_ground_truth.rs`, `v3_ground_truth_demo`, then the same with `V3_QUERIES` for section 3), `lucivy_core/benches/compare_tantivy.rs` (tantivy 0.25 from crates.io, not the fork) and `benches/compare_elasticsearch.py` (Elasticsearch 8.19 in a container, configured at its best: trigram analyzer, `wildcard` field). Logs and JSON next to this file.
