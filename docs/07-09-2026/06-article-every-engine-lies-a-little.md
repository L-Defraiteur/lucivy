<!-- Copie markdown de playground/blog/every-engine-lies-a-little.html (la page publiée, canonique). Pour dev.to : coller tel quel, canonical_url = la page. -->

# Every full-text engine lies a little

**93 983 Linux kernel files. Three search engines. One byte-by-byte ground truth.**

*Lucie Defraiteur · 7 September 2026*

| `search "de"` — two letters, over 93 983 files | |
|---|---|
| Truth, a scan of the files | **93 009 files** |
| Elasticsearch, trigram analyzer | **0** |
| tantivy, n-gram tokenizer | **0** |
| lucivy | **93 009** |

Here is a thing I did not expect to find: ask a search engine for the two letters `"de"` over the source of the Linux kernel, and two of the three engines I tested answer **0 documents**. Not an error, not a warning. Zero. The truth is 93 009 of 93 983 files.

I write a search engine, so I am not neutral, and I will say where it loses. But the point of this post is not my engine. It is a habit, and it fits in one box:

> **The rule.** **The files are the ground truth.** Every engine's answer is compared against a byte-by-byte scan of the same files — document ids *and* exact byte spans. Not against another engine, not against a fixture someone wrote by hand.

Once you have that habit, every engine turns out to lie a little. Not mine, today: the test that caught my own engine twice this summer now runs on every release, and it passes with zero mismatches.

## The setup

One corpus: a Linux kernel tree (7.2), 93 983 text files after filtering, 857 MB of text. Three engines, each configured the way its own documentation says to do substring search, not left at defaults that would make it a straw man:
- **Elasticsearch 8.19**, a trigram analyzer (min and max gram 3) on the content field, plus a `wildcard` field for regular expressions. The standard analyzer is measured too, for size.
- **tantivy 0.25**, upstream, with its `NgramTokenizer` (3 grams); its default tokenizer measured too. Its n-gram phrase query returns 0 because all n-gram positions are 0, so I run a boolean AND of the trigrams and re-read the stored text of each candidate to verify. That is the fairest thing I could do for it.
- **lucivy 4.0**, mine, a suffix FST over every token, defaults.

And one judge: a scan of the files. For every query, a small program reads each file and finds the matches with plain string operations or a regex, then counts the documents and, when it can, the byte offsets. The scan takes seconds per query; that is fine, it only has to be right. Everything here is reproducible with one script, `benches/compare_engines.sh`, which writes [this report](https://github.com/L-Defraiteur/lucivy/blob/main/docs/compare-engines-2026-09-05.md) with the exact mappings and tokenizer settings.

First, the boring sanity check: ordinary substrings work everywhere. `mutex_lock` (5 145 files), `spin_lock` (6 569) and `sched` (9 289) match the scan exactly on all three engines. That is the case everyone tests, and everyone passes it.

## Where they part

**93 009 → 0** — A two-character substring. Both trigram indexes return nothing, and say nothing.

**10 034 → 3 549** — A typo sitting across an underscore. Term-level fuzziness misses two thirds of the files.

**20 797 / 20 797** — Every byte position of `mutex_lock` in 5 145 files, delivered with the answer, in 15 ms.

| asked | truth | lucivy | Elasticsearch | tantivy |
|---|---|---|---|---|
| `spin_lock`, separators strict | 6 569 | ✔ 6 569 (12 ms) | ✔ 6 569 (10 ms) | ✔ 6 569 (116 ms) |
| `spin_lock`, separators relaxed | 9 552 | ✔ 9 552  (23 ms) | ▲ 6 577 | ▲ 6 601 |
| `spinlokc`, two edits, across the boundary | 10 034 | ✔ 10 034  (148 ms) | ▲ 3 549 | ▲ 6 557 |
| `spin_lock_[a-z]+`, a regex, case folded | 5 510 | ✔ 5 510  (219 ms) | ✔ 5 510  (1 ms, written `[a-zA-Z]+`) | ✘ 0 |
| `"de"`, two characters | 93 009 | ✔ 93 009  (561 ms, 7.7 M spans) | ✘ 0 | ✘ 0 |
| `"©"`, one character, in 1 878 files | 1 878 | ✔ 1 878 (6 ms) | ✘ 0 | ✘ 0 |
| `retur -ENOMEM`, a fuzzy phrase | 14 449 | ✔ 14 449  (30 ms) | ✔ 14 446  (24 ms) | — |
| positions of `mutex_lock`, 5 145 files | 20 797 | ✔ all  (15 ms) | ▲ top 200  (179 ms) | ▲ re-read  (96 ms) |

*✔ matches the scan · ▲ incomplete · ✘ nothing, silently. Timings are the engine's own, idle machine.*

What is going on in each column:
- **Strict separators.** Everyone gets this one — but look at how. Elasticsearch's trigram analyzer keeps the underscore, so it can only be strict (its relaxed answer is the 6 577 below); tantivy's n-gram tokenizer keeps it too, and its default tokenizer drops it, so it is strict *or* relaxed depending on which index you built. With lucivy, strict and relaxed are two flags on the same query over the same index: `--strict "spin_lock"` is 6 569, without it 9 552.
- **Relaxed separators.** Elasticsearch's trigrams carry the underscore, so `spin lock` and `spinlock` are other strings to it. tantivy's n-gram tokenizer is the opposite: the separator never enters the index, so "relaxed" is the only mode it has — and it cannot do strict at all.
- **The typo.** `spinlokc` is one transposition away from `spin_lock`, but the transposition straddles the underscore. Term-level fuzziness compares the query to whole terms, and no single term is within two edits of it. The count is not zero, so it looks fine.
- **The regex.** tantivy's terms are already cut into grams; there is nothing left for a regex to run on. Elasticsearch's `wildcard` field gets close and misses 70.
- **Two characters.** Shorter than a trigram: unrepresentable in both indexes. Zero, and nothing tells you.
- **Any byte.** A suffix index over bytes has no notion of "symbol": `©`, `→`, `Müller`, `-ENOMEM;` are substrings like any other. To be fair about what trigrams can do: once an engine runs them over the whole character stream, a substring of three characters or more is found whatever it holds — `pin_loc`, which starts and ends in the middle of tokens across an underscore, is 6 591 on all three (lucivy 18 ms *with* its 34 885 byte positions; Elasticsearch 79 ms and tantivy 173 ms for the count alone, positions being a second, slower step for both). What they cannot see is anything shorter than a trigram: a copyright sign present in 1 878 files is a silent zero on both, like `"de"`. The kernel holds no emoji, so I did not measure one; a lone emoji is one character and falls in the same hole.
- **Positions.** Elasticsearch recomputes highlights on the top 200 documents you ask for; tantivy re-reads the stored text of every candidate. lucivy stores positions in the index and returns all of them with the answer.

Read the zeros again. They are the interesting part. An engine that returns 6 577 where the truth is 9 552 is wrong in a way you might notice. An engine that returns **0** for two characters looks like it worked and found nothing, and nobody notices, because who checks a zero?

To be precise about the word in the title: some of these are bugs; others are consequences of the index configuration, and an Elasticsearch expert will rightly say that with `min_gram = 3` the query `"de"` is unrepresentable by design. True. From the caller's point of view the dangerous part is the same: a plausible-looking answer can still be incomplete, and the API does not distinguish "none" from "I cannot ask that".

## Where they win, because they do

This is not a hit piece. Same corpus, same machine, idle:

*Index size, and the ratio to the 857 MB of text.*

| | |
|---|---|
| tantivy, default tokenizer | 612 MB · ×0.7 |
| tantivy, n-grams | 680 MB · ×0.8 |
| Elasticsearch, standard | 781 MB · ×0.9 |
| Elasticsearch, trigrams + wildcard | 3 082 MB · ×3.6 |
| lucivy, derived_in_ram | 3 335 MB · ×3.9 |
| lucivy, default | 4 926 MB · ×5.8 |

*Time to index the corpus.*

| | |
|---|---|
| tantivy, default tokenizer | 1 s |
| tantivy, n-grams | 5 s |
| Elasticsearch, standard | 28 s |
| lucivy, default | 107 s |
| lucivy, derived_in_ram | 111 s |
| Elasticsearch, trigrams + wildcard | 123 s |

tantivy indexes this corpus in one to five seconds. Mine takes a hundred. Its index is seven times smaller. On a whole-word query it answers in 0 ms where mine takes 27. Elasticsearch does the fuzzy phrase as well as I do. If your queries are whole words, use them and be happy.

And note the difference of kind: Elasticsearch is a service you run next to your application; mine is a library that goes inside it. The index lives in your process, in your transaction if you plug your own storage, on your machine — nothing to deploy beside your service, nothing that leaves it, and the same engine runs in the browser with the data staying in the tab.

What my index buys with its size is the first table: the questions a trigram index cannot ask, and the exact position of every match delivered with the answer instead of recomputed for the top 200. Put more simply: **you decide what to ask when you ask it, not six months earlier when you picked an analyzer.** Every zero in that table is a decision taken at index time — `min_gram = 3`, a tokenizer that drops separators — that forbids a question forever. A suffix index takes no such decision; strict or relaxed, exact or fuzzy, regex or two characters are options of the query.

## The method is the point

I did not start with the comparison. I started with a test that indexes the kernel, runs a panel of queries, and fails if any count or any span disagrees with the scan. It runs on every release. It caught my own engine lying twice this summer: once dropping documents whose fuzzy match crossed a separator (3.0.2 to 3.0.6, a whole month), once returning a stale dictionary one run in three under a race. Both were invisible in the ordinary tests, which showed "20 hits" because 20 was the result limit.

If you take one thing from this post: put a scan of the files next to your engine, on a real corpus, and diff. It is a slow, dumb program. It is also the only one in the room that cannot be wrong about what is in the files.

## Reproduce it

```bash
git clone --depth=1 --branch v7.2 https://github.com/torvalds/linux /tmp/linux-bench
V3_CORPUS=/tmp/linux-bench V3_SFX_VERSION=4 cargo test --release -p lucivy-core \
    --test test_sfx_v3_ground_truth v3_ground_truth_demo -- --ignored --nocapture
# the three-engine comparison, Elasticsearch optional (a container):
benches/compare_engines.sh /tmp/linux-bench
```

Another kernel tree gives other counts; what does not move is the claim: the harness fails on any disagreement between the engine and the scan of *your* files.

## If you want to try the engine

lucivy is a Rust library, MIT, with Python, Node.js and C++ bindings, and it runs in the browser: [the playground](https://l-defraiteur.github.io/lucivy/) clones its own source from GitHub and indexes it in your tab in about three seconds, then you can type `index postgres` or `index linux` and search a whole tree with `--fuzzy`, `--regex`, `--strict`. The repository is [here](https://github.com/L-Defraiteur/lucivy). And if you know a configuration of Elasticsearch or tantivy that gets closer on any row above, tell me: it goes into the report with your name on the line.
