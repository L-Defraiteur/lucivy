# 11 — Implementation LUCE snapshot (export/import)

## Session du 8 mars 2026

### Travail effectue

#### 1. Format LUCE (Lucivy Unified Compact Export)

Format binaire custom pour snapshots d'index lucivy. Design documente dans `10-snapshot-format-design.md`.

- Magic `"LUCE"` (4 octets) + version u32 LE + num_indexes u32 LE
- Chaque index : path UTF-8 + liste de fichiers (name + data bruts)
- Tout en little-endian, pas de padding, pas de compression (les segments tantivy sont deja en lz4 en interne)
- Support multi-index dans un seul blob
- ~8 octets overhead par fichier (plus compact que tar qui a 512 octets/fichier)

#### 2. Garde-fou : commit obligatoire avant export

- Ajout de `has_uncommitted: AtomicBool` dans `LucivyHandle`
- Methodes `mark_uncommitted()`, `mark_committed()`, `has_uncommitted()`
- `add`, `add_many`, `delete`, `update` → `mark_uncommitted()`
- `commit`, `rollback` → `mark_committed()`
- Export refuse avec erreur explicite si uncommitted : *"index '...' has uncommitted changes — call commit() before export"*

#### 3. Core : `lucivy_core/src/snapshot.rs`

| Fonction | Role |
|----------|------|
| `export_snapshot(indexes)` | Serialise N indexes en blob LUCE |
| `import_snapshot(data)` | Deserialise blob → liste de (path, fichiers) |
| `check_committed(handle, path)` | Erreur si uncommitted |
| `read_directory_files(path)` | Lit tous les fichiers d'un repertoire (exclut `.lock`, `.managed.json`, etc.) |

Fichiers exclus des snapshots : `.lock`, `.tantivy-writer.lock`, `.lucivy-writer.lock`, `.managed.json` (lock files tantivy + fichier interne ManagedDirectory, recrees a l'ouverture).

#### 4. Python binding

Methodes ajoutees sur `Index` :
- `export_snapshot()` → `bytes`
- `export_snapshot_to(path)` → ecrit fichier
- `Index.import_snapshot(data, dest_path=None)` → `Index`
- `Index.import_snapshot_from(path, dest_path=None)` → `Index`

Fonctions module :
- `lucivy.export_snapshots([idx1, idx2])` → `bytes` (multi-index)
- `lucivy.import_snapshots(data, dest_paths=None)` → `list[Index]`

#### 5. Node.js binding

Methodes ajoutees sur `Index` :
- `exportSnapshot()` → `Buffer`
- `exportSnapshotTo(path)`
- `Index.importSnapshot(data, destPath?)` → `Index` (factory)
- `Index.importSnapshotFrom(path, destPath?)` → `Index` (factory)

Note : le `index.js` wrapper pointait vers l'ancien `.node` (`lucivy.linux-x64-gnu.node`), mis a jour vers `lucivy.node`.

#### 6. C++ binding (CXX bridge)

Methodes ajoutees :
- `export_snapshot()` → `rust::Vec<uint8_t>`
- `export_snapshot_to(path)`
- `lucivy_import_snapshot(data, dest_path)` → `Box<LucivyIndex>`
- `lucivy_import_snapshot_from(path, dest_path)` → `Box<LucivyIndex>`

### Resultats des tests

| Binding | Tests | Resultat |
|---------|-------|----------|
| Core (snapshot.rs) | 8 tests unitaires (roundtrip vide/single/multi, bad magic, bad version, truncated, empty files) | **8/8 PASSED** |
| Python | 71 pytest (64 existants + 7 snapshot : roundtrip, file export/import, uncommitted error, multi-index, search apres import, rollback flag) | **71/71 PASSED** |
| Node.js | test.mjs (existants + snapshot roundtrip, file export/import, uncommitted error) | **PASSED** |
| C++ | test.cpp (existants + snapshot roundtrip, file export/import, uncommitted error) | **PASSED** |

### Fichiers modifies/crees

| Fichier | Changement |
|---------|------------|
| `lucivy_core/src/snapshot.rs` | **nouveau** — format LUCE, serialize/deserialize, 8 tests |
| `lucivy_core/src/lib.rs` | ajoute `pub mod snapshot;` |
| `lucivy_core/src/handle.rs` | ajoute `has_uncommitted: AtomicBool`, methodes mark/check |
| `bindings/python/src/lib.rs` | ajoute export/import snapshot, tracking uncommitted |
| `bindings/python/tests/test_lucivy.py` | ajoute 7 tests classe `TestSnapshot` |
| `bindings/nodejs/src/lib.rs` | ajoute export/import snapshot, tracking uncommitted |
| `bindings/nodejs/test.mjs` | ajoute tests snapshot |
| `bindings/nodejs/index.js` | corrige import `.node` |
| `bindings/cpp/src/lib.rs` | ajoute export/import snapshot, tracking uncommitted |
| `bindings/cpp/test.cpp` | ajoute phases 3-5 (snapshot tests) |
| `docs/7-mars-2026-08h35/10-snapshot-format-design.md` | **nouveau** — design doc format LUCE |

#### 7. WASM binding

Methodes ajoutees sur `Index` :
- `exportSnapshot()` → `Uint8Array`
- `Index.importSnapshot(data, path)` → `Index` (factory statique)
- Tracking uncommitted : `mark_uncommitted()` sur add/addMany/remove/update, `mark_committed()` sur commit/rollback
- Helper `collect_snapshot_files()` filtre les lock files via `EXCLUDED_FILES`

Tests ajoutes dans `test-worker-direct.js` : export/import roundtrip, LUCE magic check, search apres import, uncommitted error.

#### 8. Emscripten binding

Fonctions C FFI ajoutees :
- `lucivy_export_snapshot(ctx, &out_len)` → `*const u8` (thread-local buffer, pas de malloc/free)
- `lucivy_import_snapshot(data, len, path)` → `*mut LucivyContext`
- `out_len` est `*mut u32` (taille fixe 4 octets, pas `usize`)
- Tracking uncommitted : meme pattern que WASM

Le pointeur retourne par `export_snapshot` est valide jusqu'au prochain appel (pattern `thread_local` identique a `RETURN_BUF`). Cote JS : `Module.HEAPU8.slice(ptr, ptr + len)` pour copier le blob.

Tests ajoutes dans `test-node.mjs` : export/import roundtrip, LUCE magic check, search apres import, uncommitted null return.

### Resultats des tests (mis a jour)

| Binding | Tests | Resultat |
|---------|-------|----------|
| Core (snapshot.rs) | 8 tests unitaires | **8/8 PASSED** |
| Python | 71 pytest (64 + 7 snapshot) | **71/71 PASSED** |
| Node.js | test.mjs (existants + snapshot) | **PASSED** |
| C++ | test.cpp (existants + snapshot) | **PASSED** |
| WASM | test-worker-direct.js (+ snapshot) | **compile OK** (necessite browser) |
| Emscripten | test-node.mjs (+ snapshot) | **compile OK** (necessite emscripten toolchain) |

### Fichiers modifies/crees (complement)

| Fichier | Changement |
|---------|------------|
| `bindings/wasm/src/lib.rs` | ajoute export/import snapshot, tracking uncommitted, `EXCLUDED_FILES` |
| `bindings/wasm/test-worker-direct.js` | ajoute tests snapshot |
| `bindings/emscripten/src/lib.rs` | ajoute export/import snapshot (ptr+len), tracking uncommitted, `SNAPSHOT_BUF` thread-local |
| `bindings/emscripten/test-node.mjs` | ajoute tests snapshot |

### Implementation complete

Tous les 6 bindings ont le support snapshot LUCE :
- **Native** (Python, Node.js, C++) : teste et valide
- **In-memory** (WASM, Emscripten) : compile, tests ecrits, a valider avec les toolchains respectives
