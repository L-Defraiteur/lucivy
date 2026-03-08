# 05 — WASM threads : plan d'implementation

## Probleme

`wasm32-unknown-unknown` ne supporte pas `std::thread::spawn`. Tantivy spawn un thread "segment updater" meme avec `writer_with_num_threads(1, ...)`, ce qui fait planter le WASM en runtime :

```
System error: 'Failed to spawn segment updater thread'
```

Le build `wasm-pack` passe mais l'index crash a la creation.

## Solution : atomics + SharedArrayBuffer

Compiler avec les target features `atomics`, `bulk-memory`, `mutable-globals` pour que `std::thread::spawn` fonctionne dans le navigateur via SharedArrayBuffer + Web Workers.

### Ce qu'il faut

1. **Rust nightly** — necessaire pour `-Z build-std`
2. **`-Z build-std=std,panic_abort`** — recompile la std avec le support atomics
3. **Target features** : `RUSTFLAGS='-C target-feature=+atomics,+bulk-memory,+mutable-globals'`
4. **`wasm-bindgen-rayon`** (optionnel) — si tantivy utilise rayon pour le parallelisme, ce crate fournit le thread pool rayon dans le navigateur. A verifier si tantivy spawn via `std::thread` ou rayon
5. **Headers HTTP** : `Cross-Origin-Opener-Policy: same-origin` + `Cross-Origin-Embedder-Policy: require-corp` — deja en place dans le test Playwright

### Commande de build cible

```bash
RUSTFLAGS='-C target-feature=+atomics,+bulk-memory,+mutable-globals' \
cargo +nightly build -p lucivy-wasm \
  --target wasm32-unknown-unknown \
  --release \
  -Z build-std=std,panic_abort
```

Puis `wasm-bindgen` pour generer le JS glue (wasm-pack le fait automatiquement mais il faut verifier qu'il passe les flags).

Alternative avec wasm-pack :
```bash
RUSTFLAGS='-C target-feature=+atomics,+bulk-memory,+mutable-globals' \
wasm-pack build bindings/wasm --target web --out-dir ../../bindings/wasm/pkg \
  -- -Z build-std=std,panic_abort
```

## Etapes

### 1. Mecanisme de spawn dans ld-lucivy (FAIT)

Deux mecanismes de threading dans l'indexer :

**a) rayon ThreadPool** (`src/indexer/segment_updater.rs`)
- `pool` (segment updater) : `ThreadPoolBuilder::new().num_threads(1)` — rayon
- `merge_thread_pool` : `ThreadPoolBuilder::new().num_threads(num_merge_threads)` — rayon
- → Necessite `wasm-bindgen-rayon` pour fournir le thread pool rayon dans le navigateur

**b) std::thread** (`src/indexer/index_writer.rs:424`)
- `thread::Builder::new().name(...).spawn(...)` pour les indexing threads
- → Necessite les target features atomics pour que `std::thread::spawn` fonctionne sur wasm32

Les deux sont necessaires : atomics + wasm-bindgen-rayon.

### 2. Tester le build nightly + atomics

```bash
rustup toolchain install nightly
rustup component add rust-src --toolchain nightly
```

Puis la commande de build ci-dessus. Verifier que ca compile sans erreur.

### 3. Verifier que wasm-bindgen genere le bon JS glue

Le JS genere par wasm-bindgen avec atomics inclut du code pour :
- Charger le WASM dans un SharedArrayBuffer
- Spawner des Web Workers pour les threads Rust
- Initialiser la memoire partagee

Verifier que `lucivy_wasm.js` genere contient les hooks de thread.

### 4. Adapter le worker (`lucivy-worker.js`)

Le worker actuel fait `import init from '../pkg/lucivy_wasm.js'`. Avec les threads, l'init peut necessiter de passer le memory et le module aux sub-workers.

Points a verifier :
- Est-ce que wasm-bindgen genere un `initSync` avec shared memory ?
- Est-ce qu'il faut passer `{ module, memory }` aux threads spawnes ?
- Le worker principal doit-il etre dans un contexte qui supporte `new Worker()` nested ?

### 5. Si rayon : integrer wasm-bindgen-rayon

```toml
[target.'cfg(target_arch = "wasm32")'.dependencies]
wasm-bindgen-rayon = "1"
```

Et dans `lib.rs` du binding wasm :
```rust
#[cfg(target_arch = "wasm32")]
pub use wasm_bindgen_rayon::init_thread_pool;
```

L'appelant JS doit faire `await initThreadPool(navigator.hardwareConcurrency)` avant d'utiliser l'index.

### 6. Test Playwright

Le test Playwright existant (`test-playwright.mjs` + `test.html`) devrait fonctionner tel quel — les headers COOP/COEP sont deja en place. Verifier que :
- L'index se cree sans crash
- add/commit/search fonctionnent
- Les sub-workers se terminent proprement

## Impact utilisateur

- **Aucun** — les utilisateurs recoivent un `.wasm` + `.js` standard
- Seule contrainte : le serveur HTTP doit envoyer les headers COOP/COEP (standard pour SharedArrayBuffer)
- Navigateurs supportes : Chrome 91+, Firefox 79+, Safari 15.2+
- Pas de support IE/anciens navigateurs, mais c'est deja le cas pour WASM

## Impact build

- Nightly Rust requis pour le build WASM (les autres targets restent sur stable)
- `-Z build-std` recompile la std, premiere compilation plus lente (~2min de plus)
- A integrer dans le CI/Makefile une fois valide

## Ordre de priorite

1. ~~Etape 1 (verifier std::thread vs rayon)~~ — FAIT : les deux (rayon + std::thread)
2. Etape 2 (tester build nightly) — 10 min
3. Etape 4 (adapter le worker) — selon complexite
4. Etape 5 (wasm-bindgen-rayon si besoin) — 15 min
5. Etape 6 (test Playwright) — deja en place
6. CI/Makefile — dernier

## Etat actuel

- wasm-pack build standard : OK (compile, pkg genere)
- Runtime : crash au `Index::create` (segment updater thread)
- Test Playwright : infrastructure prete (`test.html`, `test-playwright.mjs`), en attente du fix threads
- Python binding : 64/64 tests OK
- Node.js binding : tous tests OK
