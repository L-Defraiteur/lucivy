# Roadmap : uniformisation des bindings + publication

Date : 13 mars 2026

## Contexte

Les features récentes (startsWith, BM25 scoring, BlobStore, close()) n'ont pas été propagées uniformément dans les 6 bindings. Certains bindings ont des bugs latents (writer access sans `.as_mut()` guard). Il faut uniformiser, tester, puis publier.

---

## Phase 1 — Sécuriser le writer + close() partout

### Problème

`LucivyHandle.writer` est `Mutex<Option<IndexWriter>>`. Seuls le CXX bridge et wasm-bindgen utilisent `.as_mut().ok_or("index is closed")?`. Les 4 autres (emscripten, Node.js, Python, C++) font `writer.lock()?.add_document()` directement — crash si `close()` a été appelé.

### Actions

| Binding | Adapter writer | Exposer close() |
|---------|---------------|-----------------|
| CXX bridge rag3db | ✅ déjà fait | ✅ `close_index` |
| WASM wasm-bindgen | ✅ déjà fait | ❌ ajouter `close()` |
| WASM emscripten | ❌ adapter | ❌ ajouter `lucivy_close` |
| Node.js napi | ❌ adapter | ❌ ajouter `close()` |
| Python PyO3 | ❌ adapter | ❌ ajouter `close()` |
| C++ standalone | ❌ adapter | ❌ ajouter `close()` |

### Pattern à appliquer partout

```rust
let mut guard = self.handle.writer.lock().map_err(|_| "writer lock poisoned")?;
let writer = guard.as_mut().ok_or("index is closed")?;
writer.add_document(doc)?;
```

### Emscripten spécifique

- Ajouter `_lucivy_close` dans les EXPORTED_FUNCTIONS de `build.sh`
- Le commit async thread doit aussi vérifier `as_mut()` avant commit

### Validation phase 1

- [ ] `cargo test` dans lucivy_core (71+ tests)
- [ ] `cargo test` dans ld-lucivy (1113+ tests)
- [ ] Build emscripten : `cd bindings/emscripten && bash build.sh`
- [ ] Playground : `node playground/serve.mjs` → tester create, add, search, close
- [ ] Build Node.js : `cd bindings/nodejs && npm run build`
- [ ] Build Python : `cd bindings/python && maturin develop`
- [ ] Tests Python : `cd bindings/python && pytest`

---

## Phase 2 — Propager startsWith dans tous les bindings

### État actuel

| Binding | startsWith | startsWith_split |
|---------|-----------|-----------------|
| CXX bridge | via JSON query (pas dans `search_typed_with_highlights`) | non |
| WASM emscripten | via `lucivy_search` JSON | oui (`expand_starts_with_split`) |
| WASM wasm-bindgen | via JSON query | non |
| Node.js napi | via JSON query | non |
| Python PyO3 | via JSON query | non |
| C++ standalone | via JSON query | non |

### Actions

1. **CXX bridge** : ajouter `"startsWith"` et `"startsWith_split"` dans `search_typed_with_highlights` (le mode typed)
2. **WASM wasm-bindgen, Node.js, Python, C++** : ajouter `expand_starts_with_split()` (copier le pattern d'emscripten)
3. Quand un string est passé directement comme query (shortcut), décider : garder `contains_split` par défaut ? ou migrer vers `startsWith_split` ? (startsWith est plus rapide)

### Pattern expand_starts_with_split

```rust
fn expand_starts_with_split(config: &QueryConfig) -> QueryConfig {
    let value = config.value.as_deref().unwrap_or("");
    let words: Vec<&str> = value.split_whitespace().collect();
    let field = config.field.as_deref().unwrap_or("");
    let should: Vec<QueryConfig> = words.iter().map(|w| QueryConfig {
        query_type: "startsWith".into(),
        field: Some(field.into()),
        value: Some((*w).into()),
        distance: config.distance,
        ..Default::default()
    }).collect();
    QueryConfig {
        query_type: "boolean".into(),
        should: Some(should),
        ..Default::default()
    }
}
```

---

## Phase 3 — BlobStore (optionnel par binding)

### Design

Le `BlobDirectory<S: BlobStore>` est dans lucivy_core. Chaque binding peut l'utiliser à la place de StdFsDirectory/MemoryDirectory.

### Priorités

1. **CXX bridge rag3db** : priorité haute — `CypherBlobStore` pour stocker l'index dans la DB rag3db
2. **WASM** : pas prioritaire — MemoryDirectory suffit (index en RAM)
3. **Node.js, Python** : priorité moyenne — permettrait `PostgresBlobStore` ou `S3BlobStore` via callbacks
4. **C++ standalone** : faible — StdFsDirectory suffit

### Approche Node.js / Python

Exposer un trait callback :
- Python : classe abstraite `BlobStore` que l'utilisateur implémente en Python
- Node.js : objet JS avec `load(indexName, fileName)`, `save(...)`, etc.
- Le binding Rust wrap les callbacks en `impl BlobStore`

### Pas dans cette release

BlobStore n'est pas bloquant pour la publication. On peut publier sans et l'ajouter après en minor version.

---

## Phase 4 — Tests

### Vérifications à faire

1. **Aucun test `contains` sans ngram** : vérifié dans le doc `01-fix-filter-contains-tests-ngram-mismatch.md` — les tests ont été corrigés pour inclure ngram/raw pairs
2. **Tests de close()** : ajouter dans chaque binding un test qui fait create → add → commit → close → tentative d'écriture → vérifie erreur
3. **Tests startsWith_split** : ajouter dans chaque binding un test split multi-mots
4. **Tests d'intégration emscripten** : `playground/test-playground.mjs` et `playground/test_playground.py`

### Matrice de tests par binding

| Binding | Unit tests | Integration | close() test | startsWith test |
|---------|-----------|-------------|-------------|----------------|
| CXX bridge | dans rag3db (externe) | — | à ajouter | à ajouter |
| Emscripten | pas de #[test] inline | playground/ | à ajouter | à ajouter |
| wasm-bindgen | #[test] inline | — | à ajouter | à ajouter |
| Node.js | — | tests/ (si existant) | à ajouter | à ajouter |
| Python | pytest tests/ | — | à ajouter | à ajouter |
| C++ standalone | — | bindings/cpp/test_lucivy | à ajouter | à ajouter |

---

## Phase 5 — Publication

### Versions

| Package | Version actuelle | Prochaine | Registry |
|---------|-----------------|-----------|----------|
| lucivy (Python PyPI) | 0.3.2 | 0.4.0 | PyPI via `maturin publish` |
| lucivy (npm) | 0.2.1 | 0.3.0 | npm via `npm publish` |
| lucivy (crate ld-lucivy) | — | bump Cargo.toml | crates.io (si publié) |
| WASM emscripten | pas de version package | — | copie dans playground/pkg/ |
| WASM wasm-bindgen | — | — | npm ou inclusion directe |
| C++ standalone | — | — | pas de registry |

### Changelog pour 0.4.0 / 0.3.0

- `close()` : ferme proprement l'index (commit pending + libère le lock)
- `startsWith` / `startsWith_split` : recherche prefix via FST (plus rapide que contains)
- BM25 scoring pour fuzzy/regex/startsWith (activé automatiquement avec `order_by_score`)
- Fix : writer safety après close (no crash)
- Fix : merger preserve les offsets (highlights sur segments mergés)
- Fix : UTF-8 char boundary dans contains (accents, symboles)

### Séquence de publication

1. Merger la branche `feature/startsWith` sur `main`
2. Tag la version
3. `cd bindings/python && maturin publish`
4. `cd bindings/nodejs && npm publish`
5. Rebuild emscripten et déployer playground
6. Mettre à jour les README de chaque binding

---

## Ordre d'exécution recommandé

```
Phase 1  ─── writer safety + close() ──────────── 1-2h
   │
   ├─ Emscripten: adapter writer + ajouter close
   ├─ Node.js: adapter writer + ajouter close
   ├─ Python: adapter writer + ajouter close
   ├─ C++: adapter writer + ajouter close
   └─ wasm-bindgen: ajouter close
   │
   └─ build emscripten + test playground ────────── validation

Phase 2  ─── startsWith partout ────────────────── 1h
   │
   ├─ CXX bridge: ajouter mode typed
   ├─ wasm-bindgen: ajouter expand_starts_with_split
   ├─ Node.js: ajouter expand_starts_with_split
   ├─ Python: ajouter expand_starts_with_split
   └─ C++: ajouter expand_starts_with_split

Phase 4  ─── tests ────────────────────────────── 1h
   │
   ├─ Tests close() par binding
   ├─ Tests startsWith_split par binding
   └─ Validation ngram/contains tests

Phase 5  ─── publication ──────────────────────── 30min
   │
   ├─ Merge sur main
   ├─ maturin publish (Python)
   ├─ npm publish (Node.js)
   └─ Rebuild playground
```

Phase 3 (BlobStore) est reportée à une release ultérieure — pas bloquant.
