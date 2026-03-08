# 01 — Binding C++ officiel (CXX bridge)

## Objectif

Créer un binding C++ officiel dans `bindings/cpp/`, miroir des bindings Node.js et Python.
Copie adaptée du bridge CXX existant (`lucivy_fts/rust/src/bridge.rs`) qui sert à rag3db,
sans toucher au code rag3db.

## Architecture

- **Crate** : `bindings/cpp/` → `lucivy-cpp` (MIT)
- **Dépendance** : `lucivy-fts` pour `LucivyHandle`, `query`, `tokenizer`, `directory`
- **FFI** : CXX bridge (Rust ↔ C++)
- **lib type** : `staticlib` + `lib` (comme lucivy_fts)

## API surface (même que Node.js/Python)

```
create_index(path, schema_json, stemmer) → Box<LucivyIndex>
open_index(path) → Box<LucivyIndex>
add(doc_id, fields_json)
add_many(docs_json)
delete(doc_id)
update(doc_id, fields_json)
commit()
rollback()
search(query, options_json) → Vec<SearchResult>
num_docs() → u64
get_path() → String
get_schema_json() → String
```

## Différences avec le bridge rag3db

| rag3db (`lucivy_fts/rust/bridge.rs`) | standalone (`bindings/cpp/`) |
|--------------------------------------|------------------------------|
| `_node_id` (u64) interne | `doc_id` (u64) exposé |
| Typed document ops (DocFieldText...) | JSON fields (simplicité) |
| Pas de `update`, `add_many` | API complète |
| Résultats avec `node_id` | Résultats avec `doc_id` |
| Pas de `contains_split` string | String query → contains_split |

## Fichiers à créer

- `bindings/cpp/Cargo.toml`
- `bindings/cpp/build.rs`
- `bindings/cpp/src/lib.rs` — CXX bridge
- `bindings/cpp/LICENSE` — MIT
- `bindings/cpp/include/lucivy.h` — header C++ généré par cxx
- `bindings/cpp/test.cpp` — test d'intégration
- Workspace `Cargo.toml` : ajouter `"bindings/cpp"`
