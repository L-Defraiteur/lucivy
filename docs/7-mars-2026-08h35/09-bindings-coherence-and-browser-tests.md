# 09 — Coherence bindings + tests navigateur + TypeScript

## Session du 8 mars 2026

### Travail effectue

#### 1. Browser layer emscripten (JS worker + API Promise + test Playwright)

Fichiers crees dans `bindings/emscripten/` :

| Fichier | Role |
|---------|------|
| `js/lucivy-worker.js` | Web Worker — charge le module emscripten via `import('../pkg/lucivy.js')`, gere les messages (CRUD, search, OPFS sync) |
| `js/lucivy.js` | API Promise main thread — `Lucivy` + `LucivyIndex` classes, meme interface que l'ancien binding wasm |
| `js/lucivy.d.ts` | Declarations TypeScript — `Lucivy`, `LucivyIndex`, `SearchQuery`, `SearchResult`, `FieldDef`, `SearchOptions` |
| `test.html` | Page de test navigateur avec timeout safety net |
| `test-playwright.mjs` | Runner Playwright — serveur HTTP avec COOP/COEP + Chromium headless, 60s timeout |

#### 2. Corrections build emscripten

- **`-sEXPORT_ES6=1`** : genere un vrai ES module (`export default createLucivy`) au lieu d'UMD. L'import dynamique dans le worker fonctionne proprement.
- **`PTHREAD_POOL_SIZE=8`** + **`PTHREAD_POOL_SIZE_STRICT=0`** : le pool de 4 etait insuffisant. `commit()` deadlockait car rayon spawne des threads supplementaires pendant le merge de segments. Avec 8 + croissance dynamique, tout passe.

#### 3. `addMany` ajoute a emscripten

Alignement avec les autres bindings (Python, Node.js, C++, WASM) :
- `src/lib.rs` : `lucivy_add_many(ctx, docs_json)` — accepte `docId` ou `doc_id` comme cle
- `build.sh` : export `_lucivy_add_many`
- `js/lucivy-worker.js` : handler message `addMany`
- `js/lucivy.js` : `LucivyIndex.addMany(docs)`
- `js/lucivy.d.ts` : type declaration

#### 4. Audit de coherence inter-bindings

Tous les bindings lus et compares : Python, Node.js natif, C++ (CXX bridge), WASM (wasm-bindgen), Emscripten.

### Resultats des tests

| Binding | Test | Resultat |
|---------|------|----------|
| Python | 64 pytest (CRUD, contains, fuzzy, regex, highlights, boolean, filters, persistence, edge cases) | **64/64 PASSED** |
| Node.js natif | test.mjs (create, add, search, highlights, boolean, fuzzy, regex, delete, update) | **PASSED** |
| C++ (CXX) | test.cpp compile + run (create, add, search, highlights, boolean, fuzzy, regex, filtered, delete, update, add_many, reopen) | **PASSED** |
| Emscripten Node | test-node.mjs (create, add, commit, search BM25, highlights, delete, addMany) | **PASSED** |
| Emscripten Playwright | test-playwright.mjs (Chromium headless, COOP/COEP, SharedArrayBuffer, pthreads) | **PASSED** |

### Analyse de coherence

#### Coherent partout
- `add(doc_id, fields)` — present dans les 5 bindings
- `addMany` / `add_many` — present dans les 5 bindings (emscripten ajoute cette session)
- `update(doc_id, fields)` — present partout
- `commit()` / `rollback()` — present partout
- `num_docs` — getter partout
- `schema` — present partout
- Highlights : filtre `._raw` / `._ngram` interne dans tous les bindings
- `contains_split` multi-field : meme logique dans les 5 bindings
- `auto_duplicate` (raw + ngram) : present dans les 5 bindings

#### `delete` vs `remove` — ecart accepte

| Binding | Methode | Raison |
|---------|---------|--------|
| Python | `delete(doc_id)` | Convention Python |
| Node.js natif | `delete(doc_id)` | napi permet l'utilisation |
| C++ | `remove(doc_id)` | `delete` est un mot-cle C++ |
| WASM | `remove(doc_id)` | `delete` mot reserve JS (fonctionne comme methode mais convention) |
| Emscripten | `remove(doc_id)` | Alignement avec WASM/C++ |

#### `search` unifiee vs split

| Binding | API |
|---------|-----|
| Python | `search(query, allowed_ids=None)` — un seul method |
| Node.js natif | `search(query, {allowed_ids})` — un seul method |
| C++ | `search` + `search_filtered` + variantes `_with_highlights` |
| WASM | `search` + `searchFiltered` |
| Emscripten | `search` + `searchFiltered` (C FFI ne supporte pas les args optionnels) |

#### Types doc_id

- Python / C++ : `u64`
- Node.js / WASM / Emscripten : `u32` (limitation plateforme JS)

#### Naming par plateforme (correct)

- JS : `docId` (camelCase) — napi auto-convertit `doc_id`, serde rename dans WASM/emscripten
- Python : `doc_id` (snake_case)
- C++ : `doc_id` (snake_case)

### A faire : helpers import/export snapshot

Pour chaque binding, prevoir des fonctions helper pour importer/exporter des snapshots complets d'un ou plusieurs index a la fois. Permet backup, migration, transfer entre instances.

A etudier pour chaque binding :
- **Rust natif (lucivy-core)** : fonctions au niveau `LucivyHandle` ou `Directory` pour serialiser/deserialiser tous les fichiers d'un index en un seul blob (tar/zip ou format custom). Base pour tous les autres bindings.
- **Python** : `Index.export_snapshot(path)` / `Index.import_snapshot(path)` — ecrit/lit un fichier archive sur disque. Variante multi-index.
- **Node.js natif** : idem Python, `exportSnapshot(path)` / `importSnapshot(path)`.
- **C++** : `export_snapshot(path)` / `import_snapshot(path)` via CXX bridge.
- **WASM / Emscripten** : deja partiellement present (`export_all` / `export_dirty` + `import_file`). Ajouter un helper qui produit un seul Uint8Array/base64 blob pour transfer complet. Utile pour IndexedDB, download/upload, sync entre onglets.
- **Multi-index** : possibilite d'exporter/importer N index dans un seul fichier (avec manifest). Utile pour les knowledge bases qui ont plusieurs index (par type de noeud, par langue, etc).

Points a investiguer avant implementation :
- Format du snapshot (tar simple? format custom avec header?)
- Compression (lz4/zstd deja utilise par ld-lucivy en interne)
- Streaming vs tout-en-memoire (important pour gros index)
- Coherence entre les fichiers (faut-il forcer un commit avant export?)
- Gestion des versions de schema (forward/backward compat)

### Fichiers modifies cette session

| Fichier | Changement |
|---------|------------|
| `bindings/emscripten/js/lucivy-worker.js` | **nouveau** — Web Worker emscripten |
| `bindings/emscripten/js/lucivy.js` | **nouveau** — API Promise main thread |
| `bindings/emscripten/js/lucivy.d.ts` | **nouveau** — declarations TypeScript |
| `bindings/emscripten/test.html` | **nouveau** — page test navigateur |
| `bindings/emscripten/test-playwright.mjs` | **nouveau** — runner Playwright |
| `bindings/emscripten/build.sh` | ajoute `-sEXPORT_ES6=1`, `PTHREAD_POOL_SIZE=8`, `PTHREAD_POOL_SIZE_STRICT=0`, export `_lucivy_add_many` |
| `bindings/emscripten/src/lib.rs` | ajoute `lucivy_add_many` |
| `bindings/emscripten/test-node.mjs` | ajoute test addMany |
