# Recap session — 12 avril 2026 (session 2)

## Objectif

Unifier sur `ShardedHandle` partout (bindings, snapshot, recherche),
ajouter le pre-filter par node_ids, et préparer l'infrastructure OPFS +
async pour le playground browser.

## Ce qu'on a fait

### 1. LUCE v2 — format snapshot shardé

`lucistore/src/snapshot.rs` :
- Version 2 : flag `is_sharded` + `root_files` (shard config, stats)
- Backward-compat v1 (auto-détecté au parse)
- `export_snapshot_sharded(indexes, root_files)` pour le shardé
- `import_snapshot()` retourne `ImportedSnapshot { root_files, indexes, is_sharded }`

`lucivy_core/src/snapshot.rs` :
- Refactoré pour déléguer au format lucistore (plus de code dupliqué)
- `export_to_snapshot(handle, path)` — exporte tout ShardedHandle
- `import_from_snapshot(data, dest)` — importe tout LUCE (v1 ou v2) → ShardedHandle
  - Non-shardé : wrappe dans shard_0/, génère `_shard_config.json`
  - Shardé : écrit root files + shard dirs
- Legacy `export_index`/`import_index` gardés

Tous les bindings adaptés pour `ImportedSnapshot`.

### 2. search_filtered sur ShardedHandle

- `ShardMsg::Search` porte un `filter: Option<Arc<HashSet<u64>>>`
- `execute_weight_on_shard` : pre-filter via `AliveBitSet` (pas FilterCollector)
- `ShardedHandle::search_filtered()` → `search_internal()` partagé avec `search()`

### 3. DAG conditionnel single/multi shard

`search_dag.rs` — un seul `build_search_dag`, conditionné au build-time :
- N=1 : `prescan_0 → build_weight → search_0 → output` (pas de merge)
- N>1 : `prescan_0..N ∥ → merge_prescan → build_weight → search_0..N ∥ → merge → output`

`SearchShardNode` sort `Vec<ShardedSearchResult>` directement.
`OutputNode` comme point de convergence unique.
`ShardedHandle::search()` extrait toujours de `"output"/"results"`.

### 4. Pre-filter via AliveBitSet

Vrai pre-filter : le scorer ne visite que les docs autorisés.
- `AliveBitSet::from_bitset(&BitSet)` — création depuis un BitSet en mémoire
- `SegmentReader::set_alive_bitset(custom)` — injecte + intersecte
- `build_node_filter_bitset(seg_reader, allowed_ids)` — scan fast field `_node_id`
- `execute_weight_on_shard` : clone reader + inject bitset avant scoring
- FilterCollector supprimé du chemin search_filtered

### 5. RamShardStorage

`lucivy_core/src/sharded_handle.rs` :
- In-memory storage via `RamDirectory` (pas de filesystem)
- `import_shard_file(shard_id, name, data)` — pré-peuple pour open
- `create_shard_handle` stocke le directory créé
- `open_shard_handle` utilise le directory pré-peuplé

### 6. Luciole async executor

`luciole/src/async_executor.rs` — futures dans le pool d'actors :
- `AsyncScope::new(priority)` — soumet des futures à un `AsyncActor`
- `AsyncActor` : drive les futures dans `poll_idle()`, intégré au scheduler
- `LucioleWaker` : `std::task::Wake` → `SchedulerNotifier` (réveille l'actor)
- `FutureHandle<T>` : attente coopérative ou non-bloquante
- `SignalFuture` : poll un `AtomicU32` partagé (bridge JS Promise)
- `SignalDataFuture` : idem + données résultat

Pas de thread supplémentaire, pas de runtime séparé. Les futures
héritent de la priorité de leur actor.

### 7. Bridge OPFS (SignalFuture + JS)

`bindings/emscripten/src/opfs.rs` :
- `opfs::write_async(path, data)` → `SignalFuture`
- `opfs::persist_files(scope, base_path, files)` — batch via `AsyncScope`
- FFI vers JS : `js_opfs_write`, `js_opfs_read`, `js_opfs_delete`, `js_opfs_list`

`bindings/emscripten/js/opfs-bridge.js` :
- Implémente les fonctions OPFS async
- Signale via `Atomics.store` sur la mémoire WASM partagée

### 8. WASMFS + OPFS direct

Découverte : WASMFS monte OPFS comme filesystem POSIX transparent.
`std::fs::*` → OPFS automatiquement. Plus besoin de bridge custom.

`bindings/emscripten/src/lib.rs` :
- Mount OPFS au démarrage : `wasmfs_create_opfs_backend()` + `/opfs`
- `lucivy_create` → `ShardedHandle::create("/opfs/lucivy/...")` (FsShardStorage)
- `lucivy_open` → `ShardedHandle::open(...)` directement
- `lucivy_import_snapshot` → `snapshot::import_from_snapshot(data, dest)` sur le FS
- Legacy `open_begin/import_file/open_finish` : écrit sur WASMFS puis ouvre
- Supprimé : `MemoryDirectory`, `export_dirty`, `export_all`, `rollback`

`bindings/emscripten/build.sh` : ajout `-sWASMFS`

Le code storage du binding est **identique au natif**.

## Tests

| Test suite | Résultat |
|-----------|----------|
| lucistore snapshot | 7/7 |
| lucivy-core snapshot | 10/10 |
| sharded_handle (all) | 8/8 (dont prefilter + ram_shard) |
| luciole (all) | 138/138 (dont async executor + signal) |
| lucivy-emscripten | 4/4 |

## Fichiers modifiés

### Commits poussés (branche feature/unified-sharded-handle)

- `lucistore/src/snapshot.rs` — LUCE v2 format
- `lucivy_core/src/snapshot.rs` — API unifiée
- `lucivy_core/src/sharded_handle.rs` — search_filtered, RamShardStorage, pre-filter
- `lucivy_core/src/search_dag.rs` — DAG conditionnel, OutputNode
- `lucivy_core/src/query.rs` — Debug derive
- `lucivy_core/src/handle.rs` — fix tantivy_fst, ImportedSnapshot
- `src/fastfield/alive_bitset.rs` — from_bitset()
- `src/index/segment_reader.rs` — set_alive_bitset()
- `src/lib.rs` — re-export BitSet
- `luciole/src/async_executor.rs` — nouveau : executor + SignalFuture
- `luciole/src/lib.rs` — module + re-exports
- `luciole/src/mailbox.rs` — wake_handle(), pub(crate)
- `luciole/src/scheduler.rs` — pub(crate) SchedulerNotifier
- `bindings/emscripten/src/lib.rs` — migration ShardedHandle + WASMFS
- `bindings/emscripten/src/opfs.rs` — bridge OPFS
- `bindings/emscripten/js/opfs-bridge.js` — JS glue
- `bindings/emscripten/build.sh` — -sWASMFS
- `bindings/emscripten/Cargo.toml` — dep luciole
- `bindings/{wasm,nodejs,python,cpp}/src/lib.rs` — ImportedSnapshot compat
- `docs/11-avril-2026-11h53/07-plan-unified-handle.md`
- `docs/11-avril-2026-11h53/08-plan-prefilter-node-ids.md`
- `docs/11-avril-2026-11h53/09-plan-luciole-async-executor.md`

## Prochaines étapes

1. **Binding Python** — migrer vers ShardedHandle (génère le .luce du playground)
2. **Autres bindings** (wasm-bindgen, nodejs, cpp, cxx bridge)
3. **Tester le playground** — build emscripten + test browser
4. **Demo Linux kernel** — exporter bench RR en .luce shardé
5. **Cleanup** — rendre `LucivyHandle::search()` pub(crate)
