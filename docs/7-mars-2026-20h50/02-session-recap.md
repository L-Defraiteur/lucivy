# 02 — Récap session 7 mars 2026

## Ce qui a été fait

### 1. Licence
- Tout le projet sous MIT (core + bindings)

### 2. Binding Node.js (napi-rs)
- `bindings/nodejs/` créé avec :
  - `Cargo.toml` (crate `lucivy-napi`, MIT)
  - `LICENSE` (MIT)
  - `build.rs` (napi-build)
  - `package.json` (name: lucivy)
  - `src/lib.rs` — binding complet, miroir du Python
  - `index.js` — re-export CommonJS
  - `test.mjs` — test d'intégration
- API : `Index.create/open`, `add/addMany/delete/update`, `commit/rollback`, `search`
- Search : string (contains_split), object (contains, contains+fuzzy, contains+regex, boolean, filters)
- Highlights, allowedIds, numDocs/path/schema getters
- Build release : 9.1MB .so, tous tests passent

### 3. Réorganisation bindings/
- `lucivy/` → `bindings/python/` (git mv, chemins Cargo.toml mis à jour)
- Workspace `Cargo.toml` : members mis à jour (`bindings/python`, `bindings/nodejs`)
- Les deux bindings compilent (`cargo check -p lucivy`, `cargo check -p lucivy-napi`)

### 4. Nettoyage tests Python
- Suppression tests legacy `type: "fuzzy"` standalone (3 tests)
- Suppression tests legacy `type: "regex"` standalone (4 tests)
- Remplacement `type: "fuzzy"` dans boolean par `contains + distance`
- Docstrings mises à jour (plus de mention "two approaches")
- 64/64 tests passent

### 5. CI/CD GitHub Actions
- Job `lucivy-python` : chemin corrigé `cd bindings/python`
- Job `lucivy-nodejs` ajouté : build release + copie .node + `node test.mjs`

### 6. README rewrite
- Tableau d'install en haut
- Section Node.js avec exemple complet (avant Python)
- Section Python mise à jour (nouveau chemin, 64 tests)
- Section "Per-token queries (legacy)" supprimée
- Section License simplifiee (tout MIT)
- Chemins Building mis à jour

## Commit
- `4fa8347` sur `main`, poussé sur origin
- 17 fichiers, +1115 / -193 lignes

## Prochaine étape
- Publication du package npm (prebuilds cross-platform via CI + `npm publish`)
- WASM viendra après (nécessite adapter IndexedDB/OPFS)
