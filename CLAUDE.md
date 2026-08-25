# ld-lucivy — Contexte projet

## Architecture

Moteur full-text search Rust avec substring matching via Suffix FST. Trois couches :

- **ld-lucivy** : moteur core (index, query, scoring, merger, segments, SFX engine)
- **lucivy_core** : handle unifié (`ShardedHandle`), query builder, tokenizers, snapshot/delta, blob store
- **luciole** : framework actor/DAG (crate séparé, WASM-safe)
- **lucistore** : persistance partagée (BlobStore, ShardStorage, snapshot/delta, sync)
- **sparse_vector** : index sparse (postings + WAND, `src/wand/`) sur lucistore, shardé via luciole
  (`ShardedSparseHandle`) — crate ami, MIT, code original (design inspiré de Qdrant, aucun code
  dérivé : audit ligne à ligne, voir `docs/24-08-2026/05-wand-comparaison.md`)
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
| `fuzzy` | → contains + distance | cross-token fuzzy via trigram pigeonhole ; `fuzzy_metric: "jaro_winkler"` + `min_similarity` (0.9) valide les candidats par Jaro-Winkler au lieu de Levenshtein |
| `regex` | → contains + regex=true | cross-token regex via literal extraction |
| `phrase` | → contains | multi-token adjacency |
| `startsWith` | → contains + anchor_start | SI=0 only |
| `startsWith_split` | → contains_split + anchor_start | |
| `parse` | value simple → OR de contains par mot×champ ; syntaxe booléenne (AND/OR/NOT, guillemets, +/-, parenthèses autonomes) → `boolean` de contains (NOT > AND > OR, mots côte à côte = OR) ; highlights dans les deux cas, multi-`fields` | `query_warnings` dit laquelle |
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

Fichiers par segment (v3, par champ) : `.sfx`, `.sfxpost`, `.termtexts`, `.posmap`,
`.bytemap`, `.word_sfxpost`, `.word_pos_map`, `.sibling_v3`. (`.gapmap`, `.sepmap` : v2.)

**`sfx_version` par défaut = 3** depuis le 23 août 2026. Un `meta.json` sans le champ
est un index v2 (le champ est maintenant toujours écrit). Les tests du moteur v2
utilisent `Index::create_in_ram_sfx2`.

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

## Bindings — état 3.0.0 (25 août 2026)

| Binding | Snapshot | Delta | 3.0.0 : `query_warnings`, `compact`, `wait_merges_quiet`, `index_bytes`, `drop_index`, `open_snapshot(_from)` | Filtré (`allowed_ids`) |
|---------|----------|-------|------|------|
| Python | export+import+**servi en place** | export+apply (sharded) | oui — tests `tests/test_v3_api.py` (93 verts, 4 skip documentés) | oui |
| Node.js | export+import+**servi en place** | export+apply (sharded) | oui — `tests/v3_api.mjs` | oui |
| C++ (cxx) | export+import+**servi en place** | export+apply (sharded) | oui — tests Rust dans `lib.rs` ; `rollback` = erreur honnête | oui |
| Emscripten | import only | manquant | `memory_status`, `preload`, drapeaux (`--scheduler-threads`, `--max-merged-docs`, `--max-builds`, `--ram-index-max-mb`…) | non |

Stockage blob ACID (`BlobStore`, `BlobShardStorage`, lazy) : **exposé dans
les trois bindings natifs** depuis le 25 août au soir — Python
(`Index.create_with_blob_store` / `open_with_blob_store`, objet duck-typé,
GIL relâché sur tout appel), Node (`BlobIndex`, classe asynchrone, callbacks
via `ThreadsafeFunction`), C++ (`lucivy::BlobBackend`, classe abstraite dans
`include/lucivy/blob_backend.h`). Règle : les méthodes du store tournent sur
les threads du scheduler ; thread-safe, jamais de réentrance dans l'index,
et le thread appelant ne doit pas tenir GIL / boucle d'événements.

Emscripten manque : export_snapshot, export_sharded_delta, apply_sharded_delta.

## Extension rag3db (lucivy_fts)

- `lucivy_fts/rust/src/bridge.rs` — bridge CXX Rust (dans ce repo)
- `../../lucivy_fts/` — code C++ de l'extension (repo séparé)

## Scoring

- BM25 standard, correct cross-shard (diff=0.0000 single vs 4-shard)
- Fuzzy : tiers par miss count (`miss_penalty * 1000 + bm25`). Scores négatifs voulus.
- `ExportableStats` : sérialisable (Serialize/Deserialize) pour distributed search

## Tests

- `cargo test --lib` : 1431 passed, 0 failed, 16 ignored (les 3 anciens rouges
  réparés/retirés le 23 août : invariants de l'ancien design)
- `cargo test -p lucivy-core` : tout vert sauf `bench_sharding` t01 (clone réseau) et
  t04 (sfx:false n'existe plus) — pré-existants
- Vérité terrain : `docs/BENCHMARKS.md`
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

**Dossier courant : `docs/25-08-2026/` — lire d'abord `05-recap-progression-et-a-faire.md`,
`06-architecture.md`, `07-knowledge-dump-outils.md`** (autonomes, écrits pour
remplacer la lecture de l'historique), puis `08-relecture-commits-journee.md`
(relecture critique de la journée : ce qui a été corrigé le soir et pourquoi). 01-04 sont le détail de la journée :
journal, design de la pagination, rapport de régression, rapport de journée.

Les docs sont dans `docs/` organisés par dossier daté. Convention depuis le
24 août 2026 : `JJ-MM-AAAA` (triable). Dossier courant : `24-08-2026/` —
**lire d'abord `06-recap-progression-et-a-faire.md`, `07-architecture.md`,
`08-knowledge-dump-tests-benchs.md`** (état de fin de journée, autonomes) ;
01-05 sont le détail. Dialogue avec la session rag3weaver :
`../rag3db/extension/rag3weaver/docs/23-aout-2026-20h33/`.
- `9-mai-2026-11h14/` — session courante (deadlock fix, compat layer, feature inventory)
- `24-mars-2026-20h35/` — knowledge dump complet
- `3-mai-2026-15h00/` — design pipe_to, execute_dag_async

## Style

- Ne pas mentionner Claude dans les docs ou le code
- Docs en français
- Code et commentaires en anglais

## Packages publiés

| Registre | Package | Publié | Date |
|----------|---------|---------|---------|
| PyPI | `lucivy` | **3.0.0** (wheel `cp39-abi3-manylinux_2_28_x86_64` + sdist) | 25 août 2026 |
| npm | `lucivy` | **3.0.0** (Linux x64) | 25 août 2026 |
| npm | `lucivy-wasm` | **3.0.0** (worker + pkg WASM, `bindings/emscripten`) | 25 août 2026 |
| crates.io | `ld-lucivy`, `lucivy-core`, `luciole`, `lucistore`, `sparse-vector` | **3.0.0** | 25 août 2026 |

Précédent : `ld-lucivy` / `lucivy-core` 2.0.0, `luciole` / `lucistore` 0.1.0,
PyPI 2.0.1, npm 2.0.2. Tokens de publication : `.vault/` (ignoré par git),
`source .vault/load.sh` ; npm demande un OTP en direct.

Règle : **tout le workspace porte le même numéro** (décision du 25 août) —
une seule version à retenir pour les utilisateurs.

Ordre de publication crates.io : `luciole` → `lucistore` → `ld-lucivy` →
`lucivy-core` (→ `sparse-vector`). Jamais de `cargo publish` sans le feu vert
explicite de Lucie.
