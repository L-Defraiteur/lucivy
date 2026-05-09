# ld-lucivy — Contexte projet

## Architecture

Moteur full-text search Rust avec substring matching via Suffix FST. Trois couches :

- **ld-lucivy** : moteur core (index, query, scoring, merger, segments, SFX engine)
- **lucivy_core** : handle unifié (`ShardedHandle`), query builder, tokenizers, snapshot/delta, blob store
- **luciole** : framework actor/DAG (crate séparé, WASM-safe)
- **Bindings** (5 crates) :
  - CXX bridge rag3db : `lucivy_fts/rust/src/bridge.rs`
  - WASM emscripten : `bindings/emscripten/src/lib.rs` (extern "C" + SharedArrayBuffer + pthreads)
  - Node.js napi : `bindings/nodejs/src/lib.rs` (napi-rs)
  - Python PyO3 : `bindings/python/src/lib.rs` (pyo3)
  - C++ standalone : `bindings/cpp/src/lib.rs` (cxx bridge namespace lucivy)

Note : wasm-bindgen (single-threaded) a été retiré — emscripten est le seul binding WASM.

## Query types — v2 compat layer

Toutes les queries texte passent par le SFX engine quand sfx_enabled=true.
Les anciens types sont routés automatiquement via `build_query()` dans `lucivy_core/src/query.rs`.

| Type | Route vers | Paramètres |
|------|-----------|------------|
| `contains` | natif SFX | `field, value, distance, anchor_start, exact_match, regex, strict_separators` |
| `contains_split` | natif SFX | split whitespace → boolean should de contains |
| `term` | → contains + anchor_start + exact_match | cross-token exact match |
| `fuzzy` | → contains + distance | cross-token fuzzy via trigram pigeonhole |
| `regex` | → contains + regex=true | cross-token regex via literal extraction |
| `phrase` | → contains | multi-token adjacency |
| `startsWith` | → contains + anchor_start | SI=0 only |
| `startsWith_split` | → contains_split + anchor_start | |
| `parse` | → contains (si value simple) | |
| `phrase_prefix` | → contains | prefix match dernier token |
| `boolean` | composite | must/should/must_not |
| `disjunction_max` | composite | max score sub-queries |
| `more_like_this` | TF-IDF natif | pas SFX (recommandation, pas substring) |

### Paramètres contains (QueryConfig)

- `anchor_start: bool` — SI=0 only (match au début du token)
- `exact_match: bool` — match couvre le(s) token(s) entier(s)
- `distance: u8` — Levenshtein (0=exact, >0=fuzzy via RegexContinuationQuery)
- `regex: bool` — pattern regex cross-token
- `strict_separators: bool` — valider les séparateurs entre tokens

## SFX Engine

Suffix FST avec partitionnement SI=0/SI>0 pour le substring matching.

- **SI=0** : début de token (pour anchor_start/startsWith)
- **SI>0** : suffixes (pour contains anywhere)
- **Cross-token** : `falling_walk` + `sibling_table` pour matcher à travers les frontières de tokens
- **Fuzzy** : trigram pigeonhole via RegexContinuationQuery
- **Regex** : extraction de littéraux, validation regex sur candidats

Fichiers par segment : `.sfx`, `.sfxpost`, `.termtexts`, `.gapmap`, `.sepmap`

## Sharding

- `ShardedHandle` : N shards, routing configurable
- `balance_weight=1.0` (default) : round-robin, indexation rapide
- `balance_weight=0.2` : token-aware, co-localise les documents similaires
- BM25 cross-shard : `ExportableStats` sérialisable, `merge()`, `search_with_global_stats()`
- Distributed ready : export_stats → merge → search_with_global_stats

## Formats d'échange

- **LUCE** : snapshot complet (tous les shards)
- **LUCID** : delta incrémental (1 shard)
- **LUCIDS** : delta incrémental sharded (N shards, seulement les shards modifiés)

## Persistence — Directories

| Type | Usage | I/O pattern |
|------|-------|-------------|
| StdFsDirectory | Natif + WASM/OPFS | Deferred I/O : tout en RAM jusqu'au terminate() |
| RamDirectory | Tests | Pure RAM |
| BlobDirectory | ACID (mmap + DB blob) | Extensible (Postgres, S3, etc.) |

**WASM crucial** : `FsWriter` bufferise en RAM, I/O au `terminate()` seulement.
Jamais d'I/O dans un actor handler.

## WASM — Règles critiques

- **JAMAIS de `thread::spawn`** en WASM — tout via le scheduler (actors/tasks)
- `docstore_compress_dedicated_thread: false` en WASM
- Watch callbacks inline en WASM (pas de thread)
- GC thread skip en WASM
- `WRITER_HEAP_SIZE = 15MB` en WASM (50MB natif)
- `MAXIMUM_MEMORY = 4GB` (limit 32-bit WASM)

## luciole — framework Actor/DAG

Crate séparé dans `luciole/`. WASM-safe.

- **Actor** : trait avec priorités (Idle→Critical), GenericActor avec handlers typés
- **Scheduler** : pool threads persistants, WASM compatible
- **DAG** : construction + exécution topologique, undo, checkpoint
- **StreamDag** : pipeline streaming avec drain topologique
- **pipe_to / collect_replies_to / task_pipe_to** : request-reply non-bloquant
- **execute_dag_async** : DagExecutor actor (DAG level-by-level)
- **WaitGraph** : tracking dépendances, dump mermaid/text
- **ActorActivity** : labels dynamiques (String) dans les dumps scheduler
- **BranchNode** : FONCTION pas struct (`BranchNode(|| cond)`)

## Bindings — état v2

| Binding | v2 Ready | Snapshot | Delta | Query passthrough |
|---------|----------|----------|-------|-------------------|
| Python | READY | export+import | export+apply (sharded) | JSON QueryConfig |
| Node.js | READY | export+import | export+apply (sharded) | JSON QueryConfig |
| C++ (cxx) | READY | export+import | export+apply (sharded) | JSON QueryConfig |
| Emscripten | PARTIAL | import only | manquant | JSON QueryConfig |

Emscripten manque : export_snapshot, export_sharded_delta, apply_sharded_delta.

## Extension rag3db (lucivy_fts)

- `lucivy_fts/rust/src/bridge.rs` — bridge CXX Rust (dans ce repo)
- `../../lucivy_fts/` — code C++ de l'extension (repo séparé)

## Scoring

- BM25 standard, correct cross-shard (diff=0.0000 single vs 4-shard)
- Fuzzy : tiers par miss count (`miss_penalty * 1000 + bm25`). Scores négatifs voulus.
- `ExportableStats` : sérialisable (Serialize/Deserialize) pour distributed search

## Tests

- `cargo test --lib` : 1200 tests, 0 failed, 16 ignored
- 9 ignored : merge-timing (async merge dans actor system, pas de régression)
- 7 ignored : doc tests
- Bench sharding : `bench_sharding.rs` (90K docs Linux kernel)
- Bench vs tantivy : `bench_vs_tantivy.rs`
- IMPORTANT : toujours `> /tmp/fichier.txt 2>&1`, JAMAIS `| tail`

## Build

```bash
# Tests ld-lucivy
cargo test --lib

# Tests luciole
cargo test -p luciole --lib

# Build WASM emscripten
bash bindings/emscripten/build.sh

# Playground
cd playground && node serve.mjs
```

## Docs

Les docs sont dans `docs/` organisés par dossier horodaté.
- `9-mai-2026-11h14/` — session courante (deadlock fix, compat layer, feature inventory)
- `24-mars-2026-20h35/` — knowledge dump complet
- `3-mai-2026-15h00/` — design pipe_to, execute_dag_async

## Style

- Ne pas mentionner Claude dans les docs ou le code
- Docs en français
- Code et commentaires en anglais

## Packages publiés

| Registre | Package | Version |
|----------|---------|---------|
| PyPI | `lucivy` | 0.3.2 |
| npm | `lucivy` | 0.2.1 |
| crates.io | `lucivy-core` | 0.1.1 |
