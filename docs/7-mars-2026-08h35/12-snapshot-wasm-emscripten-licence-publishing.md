# 12 — Snapshot WASM/Emscripten + Licence MIT + Publication PyPI/crates.io

## Session du 8 mars 2026 (suite du doc 11)

### Travail effectue

#### 1. Snapshot WASM binding (`bindings/wasm/src/lib.rs`)

- Tracking uncommitted : `mark_uncommitted()` sur add/addMany/remove/update, `mark_committed()` sur commit/rollback
- `exportSnapshot()` → `Vec<u8>` (retourne `Uint8Array` en JS)
- `Index.importSnapshot(data, path)` → factory statique retournant un `Index`
- Helper `collect_snapshot_files()` filtre les lock files via `EXCLUDED_FILES`
- Tests ajoutes dans `test-worker-direct.js` : roundtrip, LUCE magic, search apres import, uncommitted error

#### 2. Snapshot Emscripten binding (`bindings/emscripten/src/lib.rs`)

- Tracking uncommitted : meme pattern que WASM
- `lucivy_export_snapshot(ctx, &out_len)` → `*const u8` via `thread_local! { SNAPSHOT_BUF }` (pas de malloc/free)
- `lucivy_import_snapshot(data, len, path)` → `*mut LucivyContext`
- `out_len` est `*mut u32` (taille fixe, pas `usize`) pour eviter ambiguite cross-platform
- Le pointeur retourne est valide jusqu'au prochain appel (meme pattern que `RETURN_BUF`)
- Tests ajoutes dans `test-node.mjs` : roundtrip, LUCE magic, search apres import, uncommitted null return

#### 3. API Rust haut niveau (`lucivy_core/src/snapshot.rs`)

Fonctions de commodite ajoutees pour les utilisateurs Rust natifs :
- `snapshot::export_index(handle, path)` → `Result<Vec<u8>, String>`
- `snapshot::import_index(data, dest_path)` → `Result<LucivyHandle, String>`
- Helper interne `write_imported_files(dest, files)`

#### 4. Passage tout MIT

- `LICENSE` racine : LRSL v1.3 → MIT
- `bindings/nodejs/LICENSE`, `bindings/cpp/LICENSE`, `bindings/wasm/LICENSE` : retire mentions LRSL
- `bindings/cpp/src/lib.rs`, `bindings/nodejs/src/lib.rs` : "Official Binding under LRSL" → "Distributed under the MIT License"
- `README.md` : section Install simplifiee (plus de colonne licence), section License simplifiee ("MIT. See LICENSE.")
- `docs/7-mars-2026-20h50/02-session-recap.md` : toute mention LRSL retiree
- Zero mention LRSL restante dans le repo (verifie par grep)

#### 5. Publication PyPI

- `pyproject.toml` enrichi : metadata completes (author, keywords, classifiers, urls)
- `maturin publish` → **lucivy 0.1.0 publie sur PyPI**
- Wheel Linux x86_64 CPython 3.13 (multi-plateforme via CI a faire)
- `pip install lucivy` fonctionne

#### 6. Publication crates.io (en cours)

- `.env` ajoute au `.gitignore` (tokens API)
- `Cargo.toml` de toutes les sous-crates : `repository` mis a jour vers `github.com/L-Defraiteur/lucivy`
- `lucivy_core/Cargo.toml` : `version = "0.26.0"` ajoute sur la dep `ld-lucivy`
- 5 crates publiees sur crates.io :
  - `ld-ownedbytes` v0.26.0
  - `ld-lucivy-tokenizer-api` v0.26.0
  - `ld-lucivy-bitpacker` v0.26.0
  - `ld-lucivy-query-grammar` v0.26.0
  - `ld-lucivy-common` v0.26.0
- 5 crates restantes (rate limit 429, a reprendre) :
  - `ld-lucivy-stacker` v0.26.0
  - `ld-lucivy-sstable` v0.26.0
  - `ld-lucivy-columnar` v0.26.0
  - `ld-lucivy` v0.26.0
  - `lucivy-core` v0.1.0

#### 7. Preparation npm

- `bindings/nodejs/package.json` enrichi : author, repository, keywords
- `bindings/wasm/pkg/package.json` : renomme en `@lucivy/wasm`, metadata ajoutees
- `.d.ts` Node.js natif : a jour (snapshot methods incluses)
- `.d.ts` WASM : pas a jour (rebuild wasm-pack necessaire)

### Fichiers modifies/crees

| Fichier | Changement |
|---------|------------|
| `LICENSE` | LRSL → MIT |
| `README.md` | licence simplifiee |
| `.gitignore` | ajoute `.env` |
| `bindings/wasm/src/lib.rs` | snapshot + uncommitted tracking |
| `bindings/wasm/test-worker-direct.js` | tests snapshot |
| `bindings/emscripten/src/lib.rs` | snapshot + uncommitted tracking + SNAPSHOT_BUF |
| `bindings/emscripten/test-node.mjs` | tests snapshot |
| `lucivy_core/src/snapshot.rs` | API haut niveau export_index/import_index |
| `bindings/nodejs/LICENSE` | retire LRSL |
| `bindings/cpp/LICENSE` | retire LRSL |
| `bindings/wasm/LICENSE` | retire LRSL |
| `bindings/cpp/src/lib.rs` | retire mention LRSL |
| `bindings/nodejs/src/lib.rs` | retire mention LRSL |
| `docs/7-mars-2026-20h50/02-session-recap.md` | retire mentions LRSL |
| `bindings/python/pyproject.toml` | metadata PyPI |
| `bindings/nodejs/package.json` | metadata npm |
| `bindings/wasm/pkg/package.json` | renomme @lucivy/wasm |
| 9x `*/Cargo.toml` | repository url + version sur path deps |

### Reste a faire

- **crates.io** : publier les 5 crates restantes (attendre fin du rate limit)
- **npm natif** : `npm publish` dans `bindings/nodejs/` (pret, linux x64 seulement)
- **npm wasm** : rebuild `wasm-pack build` pour regenerer .d.ts avec snapshot methods, puis `npm publish`
- **CI multi-plateforme** : GitHub Actions pour build wheels PyPI (linux/macos/windows) et prebuilds npm
