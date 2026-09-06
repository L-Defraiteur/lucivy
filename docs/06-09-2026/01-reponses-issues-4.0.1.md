# Réponses aux six issues ouvertes après 4.0.1 — postées le 6 septembre 2026 au soir

Contexte : les brouillons du 28 août (`docs/28-08-2026/05-reponses-issues.md`)
ont été postés pour #11, #13 et #14 ; #12 et #15 n'ont **jamais reçu de
réponse** (leurs brouillons promettaient un bench et un test qui n'existaient
pas). Depuis : 4.0.0 et 4.0.1 publiées le 6 septembre, l'index ×3,7 plus petit,
le banc comparatif rejouable, la CI qui barre la publication. Tout ce qui suit a été
posté le 6 septembre au soir (compte L-Defraiteur), tel quel ; #13 reste ouverte.

Sur le ton, inchangé : dire ce qui est fait, ce qui ne l'est pas, ne rien
promettre qu'on ne tiendra pas. Trois choses ont changé de fond depuis août
et changent les réponses : **la taille** (18 Go → 4,9 Go sur le noyau, 3,3 avec
`derived_in_ram`, ce qui était l'objection réelle), **le banc** (il existe, il
est rejouable, il inclut là où les autres gagnent), et **la CI** (une release ne
part plus sur une CI rouge — c'est arrivé une fois, la 4.0.1 le dit).

---

## #12 — published benchmarks comparing lucivy vs tantivy → **fait, avec ses limites**

> This took five months longer than it should have, and the first thing to say
> is that your question had a real objection behind it that I did not want to
> answer in August: the index was **18 GB** for the Linux kernel's 857 MB of
> text. That is fixed in 4.0 — **4.9 GB**, 3.3 GB with `derived_in_ram` — and
> the rest of this comment only makes sense because of it.
>
> **What is published now.** One command, one report, both in the repository:
> `benches/compare_engines.sh` runs lucivy, tantivy 0.25 (upstream, default
> tokenizer and `NgramTokenizer`) and, optionally, Elasticsearch 8.19 on the
> same 93 983 kernel files, and **every count is judged by the same byte-by-byte
> scan of the files** — the report is
> [`docs/compare-engines-2026-09-05.md`](https://github.com/L-Defraiteur/lucivy/blob/main/docs/compare-engines-2026-09-05.md).
> The short version, with tantivy's wins first because they are real:
>
> | | tantivy 0.25 | lucivy 4.0 |
> |---|---|---|
> | index, default tokenizer / trigrams | 612 MB / 680 MB (×0.7 / ×0.8 the text) | 4 926 MB (×5.8); 3 335 MB (×3.9) with `derived_in_ram` |
> | indexing the corpus | **1 s / 5 s** | 56 s (a suffix FST per segment), 107 s (shared dictionary, the default) |
> | `sched`, whole word | **5 285 in 0 ms** (truth 5 284) | 5 284, 27 ms |
> | `schdule`, one edit, term-level | **5 ms** (3 746 documents) | 49 ms — but 5 196 documents, the truth: it also matches across token boundaries |
> | `mutex_lock`, substring | 5 145 in 107 ms (trigram AND, then the stored text re-read: its n-gram positions are all 0) | 5 145 in 15 ms, with all 20 797 spans |
> | `spin_lock` with separators relaxed (`spin lock`, `spinlock`…) | 6 601 | **9 552**, the truth |
> | `spinlokc`, two edits across the boundary | 6 557 | **10 034** |
> | `de`, two characters | 0, silently | **93 009** |
> | `spin_lock_[a-z]+`, regex | 0 (the terms are already cut) | **5 510** |
>
> So: tantivy indexes 20 to 100 times faster and its index is 7 times smaller;
> on whole-word and term-level fuzzy queries it is faster and just as right. What
> lucivy buys with its size is the other half of the table — questions the
> trigram index cannot ask (relaxed separators, fuzzy across tokens, regex, two
> characters) and the exact positions of every match, delivered with the answer.
> Each engine runs the configuration its own documentation gives for substring
> search; a purpose-built analyzer may get closer on a row, at the price of
> designing, configuring and reindexing — and I would gladly add it to the report.
>
> **What is still not measured**, because you asked for it and it is a different
> question: plain BM25 term and boolean latency, tantivy default against lucivy,
> on a prose corpus. The kernel panel has one whole-word row (`sched`: 27 ms
> against 0). If you tell me which corpus shape matters to you, I will run that
> panel — term, boolean, indexing throughput, size — with two runs on an idle
> machine, and add it to the same report, including where tantivy wins.

---

## #15 — migration guide from tantivy → **toujours pas de guide ; ce qui est vrai à la place**

> Still no guide, and I owe you a correction on my own August draft, which I
> never posted: it promised to test opening a tantivy index with lucivy. I did
> not, and 4.0 makes the answer clear without the test — **lucivy does not open
> a tantivy index.** lucivy forked tantivy 0.22 and the segment layer still
> derives from it, but an index of ours carries files tantivy never wrote (the
> suffix FST, the position maps, a per-shard dictionary since 4.0) and a
> `meta.json` with fields of its own; a tantivy directory has none of that, and
> the substring engine has nothing to work with. Re-indexing is the path, and
> it is the honest one to write down.
>
> **What migrating looks like.** `ld-lucivy` keeps most of tantivy's API for
> schema, documents, term and boolean queries; `lucivy-core` is the layer above
> (`ShardedHandle`, the JSON query builder, snapshots, the store contract) and is
> where the substring, fuzzy and regex queries live. For a codebase on the
> standard APIs the engine-level changes are small. There is no written guide
> and I am not promising one; the README of each binding and `lucivy_core`'s
> document the API as it is.
>
> **Incremental adoption** is the easy part and the one I recommend: keep your
> BM25 queries as they are, add `contains` / fuzzy / regex where they help; the
> two do not interfere, and the index format contract of 4.0 (it opens 3.0.x
> indexes, 3.0.x does not open 4.0 ones, the first commit converts) is tested
> against a fixture the published 3.0.8 wheel built. On size, since it was the
> concern behind your other issues: 4.0 is 4.9 GB for the kernel's 857 MB of
> text, 3.3 GB with `derived_in_ram` (three sidecars per segment rebuilt at
> open instead of stored: the open pays about 2 s, never a query).
>
> **The only migration help planned**, and planned rather than promised: an
> import layer that reads the documents stored in a third-party index (a
> tantivy index's stored fields first; Elasticsearch through `_source` after)
> and re-indexes them into lucivy with a schema derived from the source, so
> that a migration is one command for the indexes whose fields are stored. I
> will post here when it exists, not before.

---

## #11 — CI → **suivi court**

> A follow-up, because it is the kind of thing this issue was about. 4.0.0 was
> tagged on 6 September while the CI of that commit was red — a lint and a test
> that compiled without the default features, nothing in the engine, but red.
> The tag published anyway, because the release workflow did not depend on CI.
> It does now: a `checks` job (clippy with `-D warnings`, the lib tests with and
> without default features, `lucivy-core`, `lucivy-cpp`) gates every publish
> job, and **4.0.1** is the same engine republished through it. Both are on the
> registries; 4.0.1 is the one to install.

---

## #13 — roadmap → **suivi court**

> All of it is published now (4.0.1 on PyPI, npm and crates.io, 6 September):
> sharding, the ACID/blob store contract, the federation with equal scores, and
> the incremental sync in the browser I mentioned in #14. Two things changed
> since August that this issue would have asked about: the index is 3.7× smaller
> (a per-shard dictionary is the default), and everything the README claims about
> other engines is now measured by one reproducible command. Closing from my
> side unless you have a follow-up question.

---

## #10 — triple-field layout / index size → **suivi court**

> Following up on the part of this issue I did not answer in August, which was
> the actual blocker: the size. The layout was gone in 3.0, but the index was
> still large — 18 GB for the Linux kernel's 857 MB of text in 3.0.8. In 4.0
> (published 6 September, 4.0.1 on the registries) it is **4.9 GB**, 3.3 GB with
> `derived_in_ram`, same answers and same spans, checked against the files. For
> a 10 GB volume that is the difference between "no" and "measure your corpus":
> count on ×4 to ×6 the text, ×3.5 to ×4 with `derived_in_ram` (the open pays
> about 2 s on the kernel, never a query). The comparison with tantivy and
> Elasticsearch, sizes included, is in
> [`docs/compare-engines-2026-09-05.md`](https://github.com/L-Defraiteur/lucivy/blob/main/docs/compare-engines-2026-09-05.md).

---

## #14 — delta sync → **suivi court**

> Published: the browser-side incremental sync described above shipped in
> 4.0.0 and 4.0.1 (`lucivy-wasm` on npm). One honest note for a client that
> keeps an index across sessions: since 4.0 the default index layout has a
> per-shard dictionary, and a delta carries its generations as bundles of their
> own, so a client and a server must both be on 4.x — a 3.0.x client does not
> read a 4.0 delta, by design (the format contract is in the CHANGELOG).

---

## Après avoir posté

- #15 annonce l'outil d'import comme « planned rather than promised » (`02-import-tantivy-elasticsearch.md`) : poster sur l'issue quand il existe, pas avant.
- Une seule promesse nouvelle : le panel BM25 term/boolean contre tantivy
  **si** le demandeur nomme un corpus (#12). Ne poster que si on compte la tenir.
- #15 ne promet plus rien d'autre que la couche d'import, un jour (décision de
  Lucie : pas de portage de projet tiers, pas de guide).
- Les liens : le rapport `docs/compare-engines-2026-09-05.md` sur `main`, la
  page `l-defraiteur.github.io/lucivy` pour la démo.
