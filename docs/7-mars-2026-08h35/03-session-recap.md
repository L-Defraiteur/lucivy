# 03 — Récap session 7 mars 2026 (matin)

## Ce qui a été fait

### 1. Binding C++ officiel (`bindings/cpp/`)
- Crate `lucivy-cpp` (MIT), dépend de `lucivy-fts` pour `LucivyHandle` et `query`
- CXX bridge avec namespace `lucivy::` pour éviter les collisions de symboles avec le bridge rag3db
- `delete` renommé `remove` (mot réservé C++)
- API : `lucivy_create`, `lucivy_open`, `add`, `add_many`, `remove`, `update`, `commit`, `rollback`, `search`, `search_with_highlights`, `search_filtered`, `num_docs`, `get_path`, `get_schema`
- String query → `contains_split` multi-field automatique
- `test.cpp` : test d'intégration complet, tous tests passent
- Static lib release : 49MB
- `Cargo.toml`, `build.rs`, `LICENSE` (MIT)

### 2. CI/CD
- Job `lucivy-cpp` ajouté dans `.github/workflows/ci.yml`
- Build release + compile test C++ + run

### 3. README
- C++ ajouté dans la table d'install (MIT)
- Section C++ avec exemples et API reference
- Section Building mise à jour
- License et lineage mis à jour (Python/Node.js/C++)

### 4. Workspace
- `Cargo.toml` : ajouté `"bindings/cpp"` et `"bindings/wasm"` dans members

### 5. Binding WASM (en cours)
- Doc plan : `docs/7-mars-2026-08h35/02-bindings-wasm-opfs.md`
- Crate `lucivy-wasm` créé dans `bindings/wasm/`
- Architecture : `MemoryDirectory` (Directory synchrone en RAM) + sync OPFS côté JS aux frontières (open/commit)
- `directory.rs` : implémente le trait `Directory` en mémoire avec tracking dirty/deleted pour export vers OPFS
- `lib.rs` : API wasm-bindgen (Index avec create/open/add/remove/update/commit/search)
- `js/lucivy-worker.js` : Worker qui gère OPFS (readAllFiles, writeFiles) + message handler
- `js/lucivy.js` : API main thread Promise-based (postMessage wrapper)
- Feature flag `cxx-bridge` ajouté à `lucivy-fts` pour que le WASM puisse dépendre de `lucivy-fts` sans CXX
- Compile OK pour `wasm32-unknown-unknown`

### 6. Modification de `lucivy-fts`
- Feature `cxx-bridge` (default) : conditionne `mod bridge` et le build.rs CXX
- Permet aux crates WASM de dépendre de `lucivy-fts` sans tirer CXX
- Aucun changement de comportement pour rag3db (feature default activé)

## Point ouvert
- L'utilisateur questionne si le WASM devrait dépendre de `lucivy-cpp` plutôt que `lucivy-fts` directement, puisque `lucivy-fts` est le crate rag3db. À discuter.

## Fichiers créés/modifiés
- `bindings/cpp/Cargo.toml`, `build.rs`, `src/lib.rs`, `LICENSE`, `test.cpp`
- `bindings/wasm/Cargo.toml`, `src/lib.rs`, `src/directory.rs`, `LICENSE`
- `bindings/wasm/js/lucivy-worker.js`, `js/lucivy.js`
- `lucivy_fts/rust/Cargo.toml` (feature cxx-bridge)
- `lucivy_fts/rust/src/lib.rs` (cfg cxx-bridge)
- `lucivy_fts/rust/build.rs` (cfg cxx-bridge)
- `Cargo.toml` (workspace members)
- `.github/workflows/ci.yml` (job lucivy-cpp)
- `README.md` (section C++)
- `docs/7-mars-2026-08h35/01-bindings-cpp-cxx.md`
- `docs/7-mars-2026-08h35/02-bindings-wasm-opfs.md`
