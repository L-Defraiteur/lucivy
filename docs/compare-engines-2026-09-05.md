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
| `spin_lock_[a-z]+` | regex, case folded | 5 510 | **5 510** OK | 24 368 | 219 ms | **5 510** · 1 ms (corrected 12 September, note below) | 0 · 0 ms |

lucivy's time is the search alone (documents and every span); Elasticsearch's is its own `took`, first run of each query; tantivy's is the count, or for substrings the whole verified path (see §3). Whole-word and prefix counts depend on each engine's definition of a word: lucivy's harness counts `sched` bounded by separators on both sides; the standard analyzer keeps `sched_clock` as one term and splits on `/`, so its whole-word and prefix rows are close but not equal. Elasticsearch runs the substring rows on its trigram index and the whole-word, prefix and fuzzy rows on its standard one; tantivy likewise. A fuzzy row that is not bold is not a miscount: their fuzziness compares whole terms, lucivy's a substring that may cross a separator — the questions differ, and the row shows by how much.

**Correction, 12 September 2026.** This report first read `5 440 · 480 ms` for Elasticsearch on the
regex row, and the READMEs said it fell `70 short`. That was our pattern, not a defect of the
`wildcard` field. The truth of the row is case-insensitive (the harness folds case, as lucivy's
engine does), and Lucene's `case_insensitive: true` folds a pattern's **literals** but not its
**character classes**: `spin_lock_` matched `SPIN_LOCK_` while `[a-z]+` refused `UNLOCKED`, so the
70 documents whose only match is uppercase were correctly left out. Written `[a-zA-Z]+`, or with no
class at all, Elasticsearch returns 5 510 — every document, in 1 ms on a warm index (the 480 ms we
published was a first, cold execution). Checked against the bytes Elasticsearch itself holds: its
93 983 stored values scanned, 5 440 case-sensitive, 5 510 case-insensitive, zero missed, zero extra.
It reproduces on a `keyword` field too, so it is not the field type. Four-line repro and the whole
verification: `docs/12-09-2026/01-elasticsearch-regex-casse.md`.

## 3. Where the questions differ

| what is asked | truth (scan) | lucivy | Elasticsearch | tantivy |
|---|---|---|---|---|
| `spin_lock`, separators strict | 6 569 | **6 569** OK, 34 667 spans, 12 ms | **6 569** (spin_lock, separators strict, 10 ms) | **6 569** (spin_lock (substring), 117 ms)<br>**6 569** (spin_lock (trigrams, verified, strict), 116 ms) |
| `spin_lock`, separators relaxed — also `spin lock`, `spin-lock`, `spinlock` | 9 552 | **9 552** OK, 55 263 spans, 23 ms | 6 577 (spin_lock, separators relaxed (spin_lock, spin lock, spin-lock, spinlock), 5 ms)<br>173 ("spin lock" as a phrase, standard analyzer, 1 ms) | 6 577 (spinlock (trigrams, verified; must find spin_lock too), 115 ms)<br>6 601 ("spin lock" (phrase, default tokenizer), 1 ms) |
| `spinlokc`, two edits, across the token boundary | 10 034 | **10 034** OK, 57 261 spans, 148 ms | 3 549 (spinlokc, two edits, across the token boundary, 25 ms) | 6 557 (spinlokc (fuzzy, 2 edits, across the boundary), 16 ms) |
| `spin_lock_[a-z]+`, a regex, case folded | 5 510 | **5 510** OK, 24 368 spans, 219 ms | **5 510** (regex on the wildcard field, written `[a-zA-Z]+`, 1 ms) | 0 (spin_lock_[a-z]+ (regex, terms), 0 ms) |
| `ude`, three characters | 69 245 | **69 245** OK, 466 094 spans, 93 ms | **69 245** (ude (three characters), 0 ms) | **69 245** (ude (three characters), 0 ms) |
| `de`, two characters | 93 009 | **93 009** OK, 7 695 534 spans, 561 ms | 0 (de (two characters), 1 ms) | 0 (de (two characters), 0 ms) |
| `retur -ENOMEM`, a fuzzy phrase (one edit: a letter missing) | 14 449 | **14 449** OK, 32 119 spans, 30 ms | 14 446 (retur -ENOMEM (fuzzy phrase: span_near of a fuzzy span and a term), 24 ms) | — |

Read across a row: the same question, what each engine can make of it (an Elasticsearch time here may be a cache hit: the same query already ran in §2). (its trigrams carry them); tantivy's default tokenizer cannot keep them (the separator never enters the index), and its n-gram tokenizer emits every position as 0, so its substring rows are an AND of trigrams verified by reading each candidate's stored text — the application's work, timed here as such. Both engines' fuzziness stops at their token boundary. An n-gram index has nothing to look up below three characters. The fuzzy phrase is the case Elasticsearch handles well, with `span_near`.


## 3 bis. Probed on 7 September 2026 — separators, symbols, mid-token starts

Twelve more substrings, asked strictly, on the same indexes (Elasticsearch `cmp_ngram`, a `match_phrase` on the trigram field; tantivy's trigram index through the verified path of §3; lucivy's dictionary index rebuilt in 107.9 s). The truth is the same scan. Bold = matches the scan. **lucivy's time includes every byte position of every match** (the span counts are in its column); the other two times are the document count alone — their positions are a second step, priced in §4 (Elasticsearch: `highlight` on the top 200; tantivy: re-reading each stored text).

| asked (strict) | truth | lucivy | Elasticsearch, trigrams | tantivy, trigrams verified |
|---|---|---|---|---|
| `de`, two characters | 93 009 | **93 009**, 565 ms, 7 695 534 spans | 0, 5 ms | 0 (no trigram) |
| `©`, one character, a symbol | 1 878 | **1 878**, 6 ms, 2 076 spans | 0, 1 ms | 0 (no trigram) |
| `→`, one character | 34 | **34**, 5 ms | 0, 1 ms | 0 (no trigram) |
| `Müller` | 3 | **3**, 8 ms | **3**, 36 ms | **3** (5 candidates) |
| `naïve` | 3 | **3**, 5 ms | **3**, 6 ms | **3** |
| `pin_loc` — starts and ends mid-token, across the underscore | 6 591 | **6 591**, 18 ms, 34 885 spans | **6 591**, 79 ms | **6 591**, 173 ms (8 352 candidates re-read) |
| `utex_loc` | 5 170 | **5 170**, 11 ms | **5 170**, 52 ms | **5 170**, 99 ms |
| `int i` — a space inside | 25 036 | **25 036**, 17 ms | **25 036**, 30 ms | **25 036**, 563 ms (45 359 candidates re-read) |
| `return -ENOMEM;` | 14 411 | **14 411**, 17 ms | **14 411**, 88 ms | **14 411**, 225 ms |
| `spin_lock(&` | 2 538 | **2 538**, 12 ms | **2 538**, 29 ms | **2 538**, 75 ms |
| `->next` | 1 972 | **1 972**, 18 ms | **1 972**, 15 ms | **1 972**, 56 ms |
| `#include <linux/` | 40 697 | **40 697**, 72 ms | **40 697**, 55 ms | **40 697**, 455 ms (40 741 candidates re-read) |

What this settles. Once an engine is set up with trigrams over the whole character stream, a substring of three characters or more is found whatever it contains — a separator, a space, an accent, a start in the middle of a token: `pin_loc` is right on all three. The structural failures are the ones §3 already shows: **anything under three characters is a silent zero** (`de`, and a copyright sign present in 1 878 files), fuzziness across a boundary, a regex on cut terms, and the price of positions. tantivy's verified path is correct by construction and pays for it in reading: the AND of common trigrams keeps 40 741 of 93 983 documents for `#include <linux/`, and every one of them is re-read. The kernel tree holds no emoji, so none was measured; a single emoji is one character and falls in the first class, an emoji inside a longer needle in the second. Probe: `CMP_CORPUS=… CMP_PROBE='a|b' cargo test --release -p lucivy-core --test compare_tantivy probe_substrings -- --ignored --nocapture`; the Elasticsearch side was twelve `match_phrase` calls; lucivy the harness with `V3_QUERIES`.

## 4. The price of knowing where

| engine | documents | spans reported | in how many documents | time |
|---|---|---|---|---|
| lucivy (every document, every span, verified) | 5 145 | 20 797 | all 5 145 | 15 ms |
| Elasticsearch, `highlight` on the top 200 | 5 145 | 2 490 (as marked by the engine) | 200 | 179 ms + 0.4 ms to parse 2.9 MB of markup |
| tantivy, AND of trigrams verified on the stored text, occurrences in the first 200 | 5 145 | 804 | 200 | 96 ms (the whole path) |

`mutex_lock`, separators strict. lucivy's spans come out of the index with the documents. Elasticsearch re-reads and re-analyses each hit's stored text (`highlight`), priced on the top 200. tantivy's trigram index has no usable positions (its n-gram tokenizer emits 0 for every token, so a trigram phrase matches nothing): the honest path is an AND of trigrams, then reading every candidate's stored text to verify the substring and count its occurrences — the time shown is that whole path, occurrences counted in the first 200 verified documents.

## How this was produced

`benches/compare_engines.sh <corpus>`: lucivy's ground-truth harness (`lucivy_core/tests/test_sfx_v3_ground_truth.rs`, `v3_ground_truth_demo`, then the same with `V3_QUERIES` for section 3), `lucivy_core/benches/compare_tantivy.rs` (tantivy 0.25 from crates.io, not the fork) and `benches/compare_elasticsearch.py` (Elasticsearch 8.19 in a container, configured at its best: trigram analyzer, `wildcard` field). Logs and JSON next to this file.
