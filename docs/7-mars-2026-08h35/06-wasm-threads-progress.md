# 06 — WASM threads : progression

## Contexte

lucivy-wasm (`wasm32-unknown-unknown` + wasm-bindgen) crashait au runtime :
```
System error: 'Failed to spawn segment updater thread'
```

ld-lucivy utilise deux mecanismes de threading :
- **rayon** (`segment_updater.rs`) : 2 ThreadPools (segment updater + merge)
- **std::thread** (`index_writer.rs:424`) : `thread::Builder::spawn` pour les indexing workers

Sur `wasm32-unknown-unknown`, ni `std::thread::spawn` ni rayon ne fonctionnent nativement.

## Approche choisie

`wasm-bindgen-rayon` pour tout faire passer par rayon, qui fournit un thread pool via Web Workers dans le navigateur.

### Fait

1. **Patch `index_writer.rs`** — ajout d'un type `WorkerHandle` qui abstrait :
   - Native : `std::thread::JoinHandle`
   - wasm32 : `crossbeam_channel::Receiver` (rayon spawn + oneshot channel)
   - `add_indexing_worker()` utilise `rayon::spawn` sur wasm32 au lieu de `thread::Builder::spawn`

2. **`wasm-bindgen-rayon`** ajoute dans `bindings/wasm/Cargo.toml`
   - Export `pub use wasm_bindgen_rayon::init_thread_pool;` dans `bindings/wasm/src/lib.rs`

3. **Worker JS** (`lucivy-worker.js`) — appelle `initThreadPool(numThreads)` apres `init()`

4. **Compilation OK** :
   - `cargo check -p ld-lucivy` (natif) : OK, pas de regression
   - Build WASM nightly + atomics : OK
   ```bash
   RUSTFLAGS='-C target-feature=+atomics,+bulk-memory,+mutable-globals' \
   cargo +nightly build -p lucivy-wasm --target wasm32-unknown-unknown --release \
     -Z build-std=std,panic_abort
   ```
   - `wasm-bindgen --target web` : OK, `initThreadPool` present dans le JS glue

### Bloque

Le WASM module n'a pas de **shared memory** (`SharedArrayBuffer`). L'erreur au runtime :
```
Failed to execute 'postMessage' on 'Worker': #<Memory> could not be cloned.
```

`wasm-bindgen-rayon` a besoin que la `WebAssembly.Memory` soit creee avec `shared: true`. Le `.wasm` compile avec `+atomics` devrait avoir le flag shared dans la memory section, mais `wasm-bindgen` ne semble pas le detecter ou le propager.

### A investiguer

1. Verifier que le `.wasm` raw (avant wasm-bindgen) a bien le flag shared dans la memory section
2. Si oui, c'est un probleme de version wasm-bindgen (0.2.108) — les versions recentes gerent mieux shared memory
3. Si non, il manque peut-etre un flag de link (`-Clink-arg=--shared-memory` ou equivalent)
4. Alternative : forcer `WebAssembly.Memory({ shared: true, initial: N, maximum: N })` dans le JS glue

### Reference : comment rag3weaver fait

rag3weaver utilise `wasm32-unknown-emscripten` (pas `wasm32-unknown-unknown`). Emscripten fournit pthreads nativement via Web Workers, donc `std::thread::spawn` et rayon marchent directement sans `wasm-bindgen-rayon`. Le build :
```bash
EMCC_CFLAGS="-pthread -fexceptions"
RUSTFLAGS="-C target-feature=+atomics,+bulk-memory,+mutable-globals -C panic=abort"
cargo +nightly build --target wasm32-unknown-emscripten -Z build-std=std,panic_abort
```

lucivy-wasm est sur `wasm32-unknown-unknown` parce que c'est un binding standalone (pas d'infra emscripten requise pour l'utilisateur).

## Fichiers modifies

| Fichier | Changement |
|---------|-----------|
| `ld-lucivy/src/indexer/index_writer.rs` | `WorkerHandle` abstraction + rayon spawn sur wasm32 |
| `bindings/wasm/Cargo.toml` | `wasm-bindgen-rayon = "1"` |
| `bindings/wasm/src/lib.rs` | `pub use wasm_bindgen_rayon::init_thread_pool;` |
| `bindings/wasm/js/lucivy-worker.js` | import + appel `initThreadPool` |

## Autres resultats de la session

- **lucivy-core extraction** : terminee, tous les 6 crates compilent
- **Bug `_config.json`** : corrige — `ManagedDirectory` GC supprimait le fichier. Fix : ecrire la config AVANT `Index::create` sur le raw Directory, et stocker la config dans `LucivyHandle.config`
- **Python** : 64/64 tests OK (y compris persistence/reopen)
- **Node.js** : tous tests OK
- **C++ binding** : pas reteste (pas d'infra C++ dispo)
- **Test Playwright** : infrastructure prete (`test.html`, `test-playwright.mjs`), bloque sur shared memory
