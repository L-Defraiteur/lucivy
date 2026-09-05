# lucivy against Elasticsearch and tantivy — one corpus, one truth

Corpus: 93 983 files, 857 MB of text (text files of 100 KB at most, no binaries, the same selection for every engine). The truth of every row is a byte-by-byte scan of the files by lucivy's ground-truth harness; a lucivy count is only reported `OK` when its documents **and** its byte spans match that scan. A count in bold equals the truth.

## 1. Index size and indexing time

| engine | how it answers a substring | index | × text | indexing |
|---|---|---|---|---|
| Elasticsearch 8.19, standard analyzer | it does not (whole words) | 781 MB | ×0.9 | 28 s |
| Elasticsearch 8.19, trigram analyzer + `wildcard` field | trigram phrases; regex on the wildcard field | 3 082 MB | ×3.6 | 123 s |
| tantivy 0.25, default tokenizer | it does not (whole words) | 612 MB | ×0.7 | 1 s |
| tantivy 0.25, `NgramTokenizer` (trigrams) | trigram phrases (positions all 0: candidates only) | 680 MB | ×0.8 | 5 s |
| lucivy 4.0, a dictionary per segment (`sfx_version` 3) | suffix FST, exact spans | 6 617 MB | ×7.7 | reused |
| lucivy 4.0, shared dictionary per shard | suffix FST, exact spans | 4 926 MB | ×5.8 | reused |
| lucivy 4.0, shared dictionary + `derived_in_ram` | suffix FST, exact spans | 3 335 MB | ×3.9 | reused |

## 2. The nine verified queries

| query | mode | truth (scan) | lucivy | spans | lucivy | Elasticsearch | tantivy |
|---|---|---|---|---|---|---|---|
| `mutex_lock` | substring | 5 145 | **5 145** OK | 20 797 | 15 ms | **5 145** · 23 ms | **5 145** · 107 ms |
| `mutex_lock` | separators relaxed | 5 825 | **5 825** OK | 22 817 | 16 ms | — | — |
| `spin_lock` | substring | 6 569 | **6 569** OK | 34 667 | 12 ms | **6 569** · 8 ms | **6 569** · 117 ms |
| `sched` | whole word | 5 284 | **5 284** OK | 27 881 | 27 ms | 1 743 · 4 ms | 5 285 · 0 ms |
| `sched` | substring | 9 289 | **9 289** OK | 53 211 | 12 ms | **9 289** · 3 ms | **9 289** · 151 ms |
| `printk` | start of token | 4 460 | **4 460** OK | 24 719 | 66 ms | 3 167 · 16 ms | 4 407 · 0 ms |
| `schdule` | fuzzy, 1 edit | 5 196 | **5 196** OK | 18 825 | 49 ms | 1 544 · 10 ms | 3 746 · 5 ms |
| `regsiter` | fuzzy, 2 edits | 34 451 | **34 451** OK | 265 797 | 793 ms | 21 321 · 26 ms | 29 291 · 16 ms |
| `spin_lock_[a-z]+` | regex | 5 510 | **5 510** OK | 24 368 | 219 ms | 5 440 · 480 ms | 0 · 0 ms |

lucivy's time is the search alone (documents and every span); Elasticsearch's is its own `took`, first run of each query; tantivy's is the count, or for substrings the whole verified path (see §3). Whole-word and prefix counts depend on each engine's definition of a word: lucivy's harness counts `sched` bounded by separators on both sides; the standard analyzer keeps `sched_clock` as one term and splits on `/`, so its whole-word and prefix rows are close but not equal. Elasticsearch runs the substring rows on its trigram index and the whole-word, prefix and fuzzy rows on its standard one; tantivy likewise. A fuzzy row that is not bold is not a miscount: their fuzziness compares whole terms, lucivy's a substring that may cross a separator — the questions differ, and the row shows by how much.

## 3. Where the questions differ

| what is asked | truth (scan) | lucivy | Elasticsearch | tantivy |
|---|---|---|---|---|
| `spin_lock`, separators strict | 6 569 | **6 569** OK, 34 667 spans, 12 ms | **6 569** (spin_lock, separators strict, 10 ms) | **6 569** (spin_lock (substring), 117 ms)<br>**6 569** (spin_lock (trigrams, verified, strict), 116 ms) |
| `spin_lock`, separators relaxed — also `spin lock`, `spin-lock`, `spinlock` | 9 552 | **9 552** OK, 55 263 spans, 23 ms | 6 577 (spin_lock, separators relaxed (spin_lock, spin lock, spin-lock, spinlock), 5 ms)<br>173 ("spin lock" as a phrase, standard analyzer, 1 ms) | 6 577 (spinlock (trigrams, verified; must find spin_lock too), 115 ms)<br>6 601 ("spin lock" (phrase, default tokenizer), 1 ms) |
| `spinlokc`, two edits, across the token boundary | 10 034 | **10 034** OK, 57 261 spans, 148 ms | 3 549 (spinlokc, two edits, across the token boundary, 25 ms) | 6 557 (spinlokc (fuzzy, 2 edits, across the boundary), 16 ms) |
| `spin_lock_[a-z]+`, a regex | 5 510 | **5 510** OK, 24 368 spans, 219 ms | 5 440 (spin_lock_[a-z]+ (regex, wildcard field), 1 ms) | 0 (spin_lock_[a-z]+ (regex, terms), 0 ms) |
| `ude`, three characters | 69 245 | **69 245** OK, 466 094 spans, 93 ms | **69 245** (ude (three characters), 0 ms) | **69 245** (ude (three characters), 0 ms) |
| `de`, two characters | 93 009 | **93 009** OK, 7 695 534 spans, 561 ms | 0 (de (two characters), 1 ms) | 0 (de (two characters), 0 ms) |
| `retur -ENOMEM`, a fuzzy phrase (one edit: a letter missing) | 14 449 | **14 449** OK, 32 119 spans, 30 ms | 14 446 (retur -ENOMEM (fuzzy phrase: span_near of a fuzzy span and a term), 24 ms) | — |

Read across a row: the same question, what each engine can make of it (an Elasticsearch time here may be a cache hit: the same query already ran in §2). (its trigrams carry them); tantivy's default tokenizer cannot keep them (the separator never enters the index), and its n-gram tokenizer emits every position as 0, so its substring rows are an AND of trigrams verified by reading each candidate's stored text — the application's work, timed here as such. Both engines' fuzziness stops at their token boundary. An n-gram index has nothing to look up below three characters. The fuzzy phrase is the case Elasticsearch handles well, with `span_near`.

## 4. The price of knowing where

| engine | documents | spans reported | in how many documents | time |
|---|---|---|---|---|
| lucivy (every document, every span, verified) | 5 145 | 20 797 | all 5 145 | 15 ms |
| Elasticsearch, `highlight` on the top 200 | 5 145 | 2 490 (as marked by the engine) | 200 | 179 ms + 0.4 ms to parse 2.9 MB of markup |
| tantivy, AND of trigrams verified on the stored text, occurrences in the first 200 | 5 145 | 804 | 200 | 96 ms (the whole path) |

`mutex_lock`, separators strict. lucivy's spans come out of the index with the documents. Elasticsearch re-reads and re-analyses each hit's stored text (`highlight`), priced on the top 200. tantivy's trigram index has no usable positions (its n-gram tokenizer emits 0 for every token, so a trigram phrase matches nothing): the honest path is an AND of trigrams, then reading every candidate's stored text to verify the substring and count its occurrences — the time shown is that whole path, occurrences counted in the first 200 verified documents.

## How this was produced

`benches/compare_engines.sh <corpus>`: lucivy's ground-truth harness (`lucivy_core/tests/test_sfx_v3_ground_truth.rs`, `v3_ground_truth_demo`, then the same with `V3_QUERIES` for section 3), `lucivy_core/benches/compare_tantivy.rs` (tantivy 0.25 from crates.io, not the fork) and `benches/compare_elasticsearch.py` (Elasticsearch 8.19 in a container, configured at its best: trigram analyzer, `wildcard` field). Logs and JSON next to this file.
