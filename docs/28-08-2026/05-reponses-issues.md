# Réponses aux six issues ouvertes — brouillons à relire avant publication

`nicolas-geysse` a ouvert six issues le **21 mars 2026**, en évaluant lucivy
pour remplacer tantivy dans un service SaaS multi-tenant. Elles sont
argumentées, précises, et sont restées **cinq mois sans réponse**.

Entre-temps, la v3 a livré la majeure partie de ce qu'il demandait.

**Rien de ce qui suit n'est publié.** Relis, corrige le ton si besoin, et je
poste sur ton feu vert.

## Sur le ton

Cinq mois de silence se mentionnent une fois, brièvement, sans se répandre en
excuses — l'excuse longue déplace l'attention sur nous alors qu'il attend des
réponses techniques. Et **on ne vend rien** : il a demandé des faits, on donne
des faits, y compris ceux qui ne nous arrangent pas (#12 et #15 ne sont pas
faits, #14 lui manque précisément là où il en a besoin).

---

## #11 — CI pipeline + automated tests → **fait**

> Sorry for the long silence on this — it was not deliberate, and the issues
> you opened turned out to describe most of what 3.0 became.
>
> CI has been in place for a while now, in two workflows:
>
> **`ci.yml`**, on every push and PR:
> - build and `cargo test --lib` over a **feature matrix** (default and
>   `--no-default-features`),
> - `cargo clippy -- -D warnings`, on the engine and on the crates we author,
> - a **ground-truth job**: a panel of strict / relaxed / fuzzy / regex queries
>   whose document counts *and byte spans* are compared to a byte-level grep of
>   the repository's own source. A missing or extra span fails the build.
> - a **thread-spawn audit** — unguarded `std::thread::spawn` outside test code
>   fails, since it would break the WASM build,
> - the Python binding built and its test suite run.
>
> **`release.yml`**, on every tag: builds on **five platforms** (Linux x86_64
> and aarch64 under manylinux_2_28, macOS x86_64 and arm64, Windows x86_64),
> runs a create/add/search smoke test where it built, attaches the artefacts to
> the release, and publishes to PyPI and npm through OIDC trusted publishing.
>
> **The gap you listed that is still open: the WASM build is not checked in
> CI.** `bindings/emscripten` needs emsdk and a nightly `-Z build-std`, so it is
> still built and published by hand. That is a real hole — it means the wasm
> package is the one artefact not reproducible from CI — and it is on the list.
>
> On regression tests for tantivy index compatibility: see #15, where I answer
> that honestly.

---

## #13 — roadmap: sharding, ACID/blobstore, distributed → **les quatre livrés**

> All four of these shipped. Briefly, with where to look:
>
> **1. Sharding.** `ShardedHandle` — N shards, parallel writers, scatter-gather
> search. Routing is configurable: `balance_weight = 1.0` is round-robin
> (fastest indexing), the default 0.2 is token-aware and co-locates similar
> documents. BM25 stays correct across shards — identical scores with 1 or 4
> shards, which is asserted in the tests, not just measured.
>
> **2. ACID with a blob store.** You implement `load` / `save` / `delete` /
> `exists` / `list`; lucivy runs on it. Your transactional store becomes the
> source of truth and the mmap cache becomes disposable. Exposed in all three
> native bindings: Python (`Index.create_with_blob_store`), Node (`BlobIndex`),
> C++ (`lucivy::BlobBackend`). S3/MinIO is exactly the intended shape.
>
> **3. Distributed shards across machines.** Each node exports its statistics,
> a coordinator merges them, every node then scores against the federation's
> corpus — nothing copied, nothing mounted. `export_stats` → `merge` →
> `search_with_global_stats`, plus a filtered variant. `test_federated_search.rs`
> asserts that the union of what two nodes return equals what a single index
> holding every document returns, **and that a document scores the same** on its
> node as in that single index.
>
> **4. Indexing in the browser.** This works: the playground clones lucivy's own
> source from GitHub and indexes it in your tab, on OPFS. 10 000 Linux kernel
> files index in 55 s in-browser, and queries answer at about 1.5× native.
> https://l-defraiteur.github.io/lucivy/
>
> Caveat worth stating for your use case: the emscripten binding can *import* a
> snapshot but cannot yet export one, nor apply a delta — see #14.

---

## #14 — delta incrémental → **fait en natif, absent du WASM**

> This exists, and it is close to the API you sketched — but not yet where you
> need it. Both halves matter, so:
>
> **What exists.** Two incremental formats next to the full `.luce` snapshot:
> **LUCID** (one shard) and **LUCIDS** (N shards, carrying only the shards that
> changed). `export_sharded_delta(&shards, &client_versions, …)` takes the
> client's per-shard versions and returns only what moved;
> `apply_sharded_delta` applies it onto an existing local index. Exposed in the
> Python, Node.js and C++ bindings.
>
> **What is missing, and it is exactly your case.** The **emscripten (browser)
> binding does not expose delta apply** — it can import a full snapshot and
> nothing else. So server-to-browser incremental sync, the use case you
> described, is not reachable from the browser today even though the machinery
> underneath is there and tested.
>
> That is a binding surface to add rather than a design problem. If this is
> still live for you, say so on this issue and I will treat it as the next thing
> on the WASM binding rather than guessing at priorities.

---

## #10 — désactiver la disposition à trois champs → **elle n'existe plus**

> Your objection was right, and v3 resolved it by removing the layout rather
> than adding a flag.
>
> There are no `._raw` / `._ngram` sub-fields any more. A v3 index declares
> exactly the fields you declare — here is the schema of an index built from a
> two-field configuration:
>
> ```
> ['_node_id', 'path', 'content']
> ```
>
> The suffix data now lives in **per-field side files** in each segment
> (`.sfx`, `.sfxpost`, `.termtexts`, `.posmap`, `.bytemap`, `.word_sfxpost`,
> `.word_pos_map`, `.sibling_v3`) instead of in extra schema fields. This is
> `sfx_version: 3`, the default since August 2026.
>
> **What I am not going to claim**: that this makes the index smaller than your
> tantivy baseline. I have not measured that comparison, and giving you a
> number I have not taken is worse than giving you none. What I can say is that
> the structure you objected to — one field becoming three — is gone.
>
> If index size is still the deciding constraint, tell me the shape of your
> corpus (document count, average size, which fields are stored) and I will
> measure it properly and publish the result, as in #12.

---

## #12 — benchmarks lucivy vs tantivy → **pas fait**

> This one is a straight no, five months on, and you were right to ask.
>
> `benches/bench_vs_tantivy.rs` exists in the repository and has **never been
> published**. What has been published is different: `docs/BENCHMARKS.md`, and
> since 3.0.7 a verified panel over **93 605 Linux kernel files** where every
> row is compared, document by document and byte span by byte span, against a
> naive scan of the same files — with that scan's own time shown next to the
> engine's, so the reference is visible rather than asserted. Nine rows, zero
> mismatches. It runs with:
>
> ```bash
> V3_CORPUS=/path/to/corpus cargo test --release -p lucivy-core \
>     --test test_sfx_v3_ground_truth v3_ground_truth_demo -- --ignored --nocapture
> ```
>
> That answers "are the answers correct and what do substring queries cost". It
> does **not** answer your actual question, which is whether standard BM25 term
> and boolean queries regress against tantivy on the same data. That number does
> not exist publicly, and I am not going to improvise it: two of our published
> figures were retracted this week — one measured on a synthetic corpus that
> made the pruning useless, one taken while the machine was saturated — so
> anything we publish now comes with its conditions, two runs, and the command
> to reproduce it.
>
> If it helps you decide, tell me which of `wiki.json` or `hdfs.json` matches
> your workload best and I will run term latency, boolean latency, indexing
> throughput and index size on both engines, and publish it including wherever
> tantivy wins.

---

## #15 — guide de migration depuis tantivy → **pas fait**

> Also a no. There is no migration guide, and your three sub-questions deserve
> straight answers rather than a promise of a document:
>
> **Can lucivy open an existing tantivy 0.25/0.26 index without re-indexing?**
> I will not answer this from memory — it is exactly the kind of claim that
> costs someone a production migration if it is wrong. I will test it against a
> real tantivy 0.25 index and reply here with what actually happens.
>
> **Cargo.toml and API differences.** lucivy is a fork of tantivy v0.26.0.
> `ld-lucivy` is the engine and keeps most of the tantivy API; `lucivy-core` is
> the layer above it (the unified handle, the query builder, sharding,
> snapshots) and is where the new capabilities live. For a ~500 LOC codebase on
> standard APIs, the engine-level changes should be small, but "should be" is
> not good enough to migrate on — see below.
>
> **Incremental adoption.** This part is genuinely easy and is the path I would
> recommend: keep your BM25 queries exactly as they are and add substring or
> fuzzy queries only where they help. The two do not interfere.
>
> Rather than write a guide from the inside — which tends to document what the
> author remembers instead of what a newcomer hits — I would rather do it with
> you: if you still have that 500-line codebase, I will port it, publish the
> diff as the migration guide, and every surprise it turns up becomes a line in
> the compatibility matrix. If the evaluation is closed on your side, say so and
> I will write it from a synthetic project instead.

---

## Après avoir posté

Deux choses en découlent directement, à ne pas oublier :

1. **Les promesses ci-dessus sont des engagements publics.** Il y en a trois :
   mesurer l'ouverture d'un index tantivy 0.25 (#15), publier le bench contre
   tantivy (#12), et exposer le delta dans le binding WASM (#14, s'il confirme
   que c'est vivant). Ne poster que si on compte les tenir.
2. **Le binding WASM est le point commun de trois issues** (#11 pas de build en
   CI, #13 pas d'export de snapshot, #14 pas de delta). C'est le trou le plus
   coûteux du projet aujourd'hui, et il se voit de l'extérieur.
