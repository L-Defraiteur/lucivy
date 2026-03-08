# 08 — Emscripten binding : succes complet

## Decision : abandon de wasm32-unknown-unknown, passage a emscripten

Apres le diagnostic du doc 07 (std::thread::park non supporte sur wasm32-unknown-unknown), on a decide d'abandonner cette target et de passer a `wasm32-unknown-emscripten`, comme rag3weaver le fait deja.

**Raison** : sur wasm32-unknown-unknown, meme avec +atomics et -Z build-std, `std::thread::park()` est stubbe et retourne `io::Error(Unsupported)`. Toutes les primitives bloquantes (crossbeam channels, oneshot, condvars) cassent. Les patchs partiels (spin-loops, skip workers, inline indexing) ne suffisaient pas — d'autres appels bloquants restaient dans le code.

Sur emscripten, TOUT marche nativement : pthreads via Web Workers, thread::park via Atomics.wait, rayon, crossbeam — zero patch necessaire.

## Revert complet de ld-lucivy

Tous les patches wasm32 dans ld-lucivy ont ete revert :
- `src/indexer/segment_updater.rs` — cfg-guard ThreadPool, rayon::spawn → **revert**
- `src/indexer/index_writer.rs` — WorkerHandle abstraction, skip workers, inline indexing → **revert**
- `src/future_result.rs` — spin-loop try_recv, from_result → **revert**
- `src/core/executor.rs` — multi_thread → SingleThread fallback → **revert**
- `src/error.rs` — debug logging → **revert**

**ld-lucivy est revenu a l'etat vanilla, zero diff dans src/**

## Nouveau binding : bindings/emscripten/

### Structure
```
bindings/emscripten/
├── Cargo.toml          # crate-type = ["staticlib"]
├── src/
│   ├── lib.rs          # extern "C" FFI (pointeurs opaques, JSON)
│   └── directory.rs    # MemoryDirectory (copie de bindings/wasm)
├── build.sh            # cargo +nightly → emcc link
├── test-node.mjs       # test Node.js
├── pkg/
│   ├── lucivy.js       # JS glue emscripten (78K)
│   └── lucivy.wasm     # module WASM (4MB)
└── js/                 # (a faire) worker + API Promise navigateur
```

### API C exportee

```c
// Lifecycle
void* lucivy_create(const char* path, const char* fields_json, const char* stemmer);
void* lucivy_open_begin(const char* path);
void  lucivy_import_file(void* ctx, const char* name, const uint8_t* data, size_t len);
void* lucivy_open_finish(void* ctx);
void  lucivy_destroy(void* ctx);

// Documents
const char* lucivy_add(void* ctx, uint32_t doc_id, const char* fields_json);
const char* lucivy_remove(void* ctx, uint32_t doc_id);
const char* lucivy_update(void* ctx, uint32_t doc_id, const char* fields_json);

// Transaction
const char* lucivy_commit(void* ctx);
const char* lucivy_rollback(void* ctx);

// File export (OPFS sync)
const char* lucivy_export_dirty(void* ctx);  // JSON: {modified:[[name,base64],...], deleted:[...]}
const char* lucivy_export_all(void* ctx);    // JSON: [[name,base64],...]

// Search
const char* lucivy_search(void* ctx, const char* query_json, uint32_t limit, int highlights);
const char* lucivy_search_filtered(void* ctx, const char* query_json, uint32_t limit, const uint32_t* ids, size_t ids_len, int highlights);

// Info
uint32_t    lucivy_num_docs(void* ctx);
const char* lucivy_schema_json(void* ctx);
```

Les fonctions retournant `const char*` utilisent un thread-local buffer (pattern rag3weaver). Le pointeur est valide jusqu'au prochain appel sur le meme thread.

### Commande de build

```bash
# Step 1: Rust → staticlib
export EMCC_CFLAGS="-pthread -fexceptions -sDISABLE_EXCEPTION_CATCHING=0"
export RUSTFLAGS="-C target-feature=+atomics,+bulk-memory,+mutable-globals -C panic=abort"
cargo +nightly build -p lucivy-emscripten \
    --target wasm32-unknown-emscripten --release \
    -Z build-std=std,panic_abort

# Step 2: emcc link
emcc liblucivy_emscripten.a -o pkg/lucivy.js \
    -pthread -sPTHREAD_POOL_SIZE=4 \
    -sALLOW_MEMORY_GROWTH=1 -sMAXIMUM_MEMORY=1GB \
    -sMODULARIZE=1 -sEXPORT_NAME=createLucivy \
    -sSTACK_SIZE=2MB \
    -sEXPORTED_FUNCTIONS='[...]' \
    -sEXPORTED_RUNTIME_METHODS='["ccall","cwrap","UTF8ToString","stringToUTF8","lengthBytesUTF8"]' \
    -sWASM_BIGINT -O2
```

Nightly requis : `rustc 1.95.0-nightly (905b92696 2026-01-31)`

### Resultats du test Node.js

```
Module loaded
Index created, ctx = 14843480
3 docs added
Committed
Num docs: 3
Search "rust programming": [{"docId":0,"score":3.774956}]
Search contains "science" with highlights: [{"docId":1,"score":0.9808292,"highlights":{"body":[[25,32]]}}]
After delete, num docs: 2

ALL TESTS PASSED
```

**Tout fonctionne** : create, add, commit, search (BM25), highlights (contains avec offsets), delete, numDocs.

## Differences avec l'ancien binding wasm-bindgen

| | wasm-bindgen (ancien) | emscripten (nouveau) |
|---|---|---|
| Target | wasm32-unknown-unknown | wasm32-unknown-emscripten |
| Threading | IMPOSSIBLE (thread::park stubbe) | Natif (pthreads via Web Workers) |
| JS glue | wasm-bindgen (classes JS) | emscripten Module.ccall/cwrap |
| File export | Uint8Array direct via JsValue | Base64 en JSON (a optimiser) |
| Taille WASM | ~3.5MB | ~4MB (+runtime emscripten) |
| Dep wasm-bindgen-rayon | Oui (ne marchait pas) | Non necessaire |
| Patches ld-lucivy | 4 fichiers modifies | Zero |

## Ce qu'il reste a faire

1. **JS worker pour navigateur** : `lucivy-worker.js` qui charge le module emscripten dans un Web Worker, communique avec le main thread via postMessage. L'ancien pattern (lucivy.js + lucivy-worker.js) peut etre adapte.

2. **Test Playwright navigateur** : adapter test-playwright.mjs pour le nouveau binding. Headers COOP/COEP necessaires (SharedArrayBuffer).

3. **Optimiser le file export** : actuellement base64 en JSON (overhead ~33%). Alternatives : SharedArrayBuffer direct, ou transferable ArrayBuffers via postMessage.

4. **Nettoyer bindings/wasm/** : soit le supprimer, soit le garder comme fallback single-thread (mais dans ce cas il faudrait finir les patches option B).

5. **Integrer au build CI** : build.sh + emsdk dans le pipeline.

6. **API Promise navigateur** : `lucivy.js` wrapper cote main thread avec Promise.

## Fichiers modifies dans cette session

| Fichier | Changement |
|---------|-----------|
| `bindings/emscripten/Cargo.toml` | **nouveau** — crate staticlib |
| `bindings/emscripten/src/lib.rs` | **nouveau** — extern "C" FFI |
| `bindings/emscripten/src/directory.rs` | **copie** de bindings/wasm/src/directory.rs |
| `bindings/emscripten/build.sh` | **nouveau** — cargo + emcc |
| `bindings/emscripten/test-node.mjs` | **nouveau** — test complet |
| `Cargo.toml` (workspace) | ajoute "bindings/emscripten" aux members |
| `bindings/wasm/Cargo.toml` | retire wasm-bindgen-rayon |
| `bindings/wasm/src/lib.rs` | retire pub use init_thread_pool |
| `bindings/wasm/js/lucivy-worker.js` | retire initThreadPool + debug logs |
| `bindings/wasm/test-worker-direct.js` | retire initThreadPool |
| `bindings/wasm/test-playwright.mjs` | retire /pkg/ rewrite |
