# 04 — Extraction de lucivy-core

## Probleme

`lucivy-fts` etait le crate rag3db mais contenait aussi le code partage par tous les bindings (handle, query, tokenizer, directory). Les bindings C++ et WASM dependaient de `lucivy-fts` alors qu'ils n'ont rien a voir avec rag3db. Le WASM avait du dupliquer ~130 lignes de `build_schema` et `configure_tokenizers` parce que `LucivyHandle::create/open` etait hardcode sur `StdFsDirectory`.

## Solution

Nouveau crate `lucivy-core` qui contient le code partage. `LucivyHandle::create` et `::open` prennent maintenant un `impl Directory` au lieu d'un chemin string, ce qui permet au WASM de passer son `MemoryDirectory` directement.

La persistence du fichier `_config.json` passe par `Directory::atomic_write/read` au lieu de `std::fs` en dur.

## Structure apres

```
lucivy_core/           code partage (nouveau)
  handle.rs            LucivyHandle generique sur Directory
  query.rs             SchemaConfig, QueryConfig, build_query
  tokenizer.rs         NgramFilter
  directory.rs         StdFsDirectory

lucivy_fts/rust/       rag3db uniquement
  bridge.rs            CXX bridge, depend de lucivy-core
  lib.rs               wrapper LucivyHandle local (orphan rule CXX)

bindings/cpp/          depend de lucivy-core (plus de lucivy-fts)
bindings/wasm/         depend de lucivy-core (plus de lucivy-fts)
                       build_schema/configure_tokenizers supprimes
bindings/python/       depend de lucivy-core (plus de lucivy-fts)
bindings/nodejs/       depend de lucivy-core (plus de lucivy-fts)
```

## Dependencies

| Crate | Depend de |
|---|---|
| lucivy-core | ld-lucivy, serde, serde_json, regex, regex-syntax |
| lucivy-fts | lucivy-core, ld-lucivy, serde_json, cxx |
| bindings/cpp | lucivy-core, ld-lucivy, serde_json, cxx |
| bindings/wasm | lucivy-core, ld-lucivy, serde, serde_json, wasm-bindgen, js-sys |
| bindings/python | lucivy-core, ld-lucivy, pyo3, serde_json |
| bindings/nodejs | lucivy-core, ld-lucivy, napi, napi-derive, serde, serde_json |

## Point technique : orphan rule CXX

Le CXX bridge dans `lucivy-fts` declare `type LucivyHandle;` dans le bloc `extern "Rust"`. CXX genere des impls pour ce type, ce qui exige qu'il soit defini dans le crate local (orphan rule). Solution : un newtype wrapper `LucivyHandle(lucivy_core::handle::LucivyHandle)` avec `Deref` dans `lucivy_fts/rust/src/lib.rs`. Les bindings C++ et WASM n'ont pas ce probleme car ils n'utilisent pas CXX avec `LucivyHandle` comme type opaque.

## Etat

- lucivy-core : compile, 48 tests passent
- lucivy-fts : compile (cargo check)
- bindings/cpp : compile (cargo check)
- bindings/wasm : compile (cargo check)
- bindings/python : migre vers lucivy-core, compile (cargo check)
- bindings/nodejs : migre vers lucivy-core, compile (cargo check)
- Anciens fichiers `lucivy_fts/rust/src/{handle,query,tokenizer,directory}.rs` supprimes

## Tests a refaire pour validation complete

1. **lucivy-core** : `cargo test -p lucivy-core` — fait, 48/48 OK
2. **lucivy-fts (rag3db)** : `cargo build -p lucivy-fts --release` puis lancer les tests e2e rag3db (`e2e_native.rs`, `e2e_phase0b.rs`) pour verifier que le bridge CXX fonctionne toujours de bout en bout
3. **bindings/cpp** : `cargo build -p lucivy-cpp --release` puis compiler et lancer `test.cpp` — verifie que le namespace `lucivy::` et toute l'API (create, add, search, highlights, filtered, schema) marchent
4. **bindings/wasm** : `cargo build -p lucivy-wasm --target wasm32-unknown-unknown` — verifie la compilation WASM effective (pas juste check). Test navigateur avec wasm-pack pas encore fait
5. **bindings/python** : `maturin develop` ou `cargo build -p lucivy --release` puis tester `import lucivy` en Python — verifie que create/open/add/search marchent avec `StdFsDirectory`
6. **bindings/nodejs** : `npm run build` ou `cargo build -p lucivy-napi --release` puis tester `require('lucivy')` en Node — meme verification
