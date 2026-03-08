# 02 — Binding WASM avec persistance OPFS

## Objectif

Créer un binding WASM dans `bindings/wasm/` qui tourne dans un Web Worker
avec persistance transparente via OPFS (Origin Private File System).

## Architecture

```
[Main thread JS]
    ↕ postMessage
[Web Worker]
    ↕ wasm-bindgen
[lucivy-wasm (Rust → WASM)]
    ↕ trait Directory
[OpfsDirectory]
    ↕ createSyncAccessHandle() — API sync, Worker only
[OPFS browser storage]
```

## Pourquoi OPFS sync dans un Worker

- Le trait `Directory` de tantivy est synchrone (read/write bloquants)
- IndexedDB est async-only → incompatible sans réécriture du moteur
- OPFS offre `createSyncAccessHandle()` : I/O synchrones, mais uniquement dans les Web Workers
- Un moteur de recherche dans un Worker est le bon pattern (pas de blocage UI)

## Ce qu'on va faire

### 1. `OpfsDirectory` — implémente `Directory` via OPFS sync

Le trait `Directory` demande :
- `open_read(path) → FileSlice` — lire un fichier entier en mémoire
- `open_write(path) → WritePtr` — écrire un fichier (buffered)
- `atomic_write(path, data)` — écriture atomique (meta.json, etc.)
- `atomic_read(path) → Vec<u8>` — lecture atomique
- `delete(path)` — supprimer un fichier
- `exists(path) → bool`
- `watch(callback)` — notification de changement (meta.json)
- `sync_directory()` — flush

Côté WASM, les appels OPFS sync passent par des imports JS
(car `createSyncAccessHandle()` est une API Web, pas accessible via std::fs) :
```rust
#[wasm_bindgen]
extern "C" {
    fn opfs_read(path: &str) -> Vec<u8>;
    fn opfs_write(path: &str, data: &[u8]);
    fn opfs_delete(path: &str);
    fn opfs_exists(path: &str) -> bool;
    fn opfs_list(dir: &str) -> Vec<String>;
}
```

Ces fonctions JS utilisent `FileSystemSyncAccessHandle` dans le Worker.

### 2. Crate `bindings/wasm/` — `lucivy-wasm`

- Target : `wasm32-unknown-unknown` + wasm-bindgen
- Dépend de `lucivy-fts` pour `LucivyHandle`, `query`
- API wasm-bindgen miroir des autres bindings :
  - `Index.create(path, fieldsJson, stemmer)`
  - `Index.open(path)`
  - `index.add(docId, fieldsJson)`
  - `index.addMany(docsJson)`
  - `index.remove(docId)`
  - `index.update(docId, fieldsJson)`
  - `index.commit()` / `index.rollback()`
  - `index.search(queryJson, limit)` → JSON results
  - `index.numDocs`, `index.path`, `index.schemaJson`

### 3. Glue JS — Worker wrapper

- `lucivy-worker.js` : instancie le WASM dans un Worker, expose l'API OPFS sync
- `lucivy.js` : API côté main thread, communique avec le Worker via postMessage
- Optionnel pour le MVP : l'utilisateur peut aussi utiliser le WASM directement dans son propre Worker

## Ordre d'implémentation

1. Scaffolding crate `bindings/wasm/` (Cargo.toml, build)
2. Fonctions d'import OPFS (JS glue + extern "C")
3. `OpfsDirectory` (impl Directory)
4. API wasm-bindgen (Index wrapper)
5. Worker wrapper JS
6. Test dans un browser (ou via playwright/wasm-pack test)
7. CI
8. README + section WASM

## Contraintes

- `createSyncAccessHandle()` : Chrome 102+, Firefox 111+, Safari 15.2+
- Worker-only (pas de main thread)
- Un seul writer par index (lock file — même contrainte que natif)
- OPFS est origin-scoped (pas de partage cross-origin)
