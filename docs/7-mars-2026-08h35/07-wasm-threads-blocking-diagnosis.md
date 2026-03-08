# 07 — WASM threads : diagnostic du blocage `std::thread::park`

## Probleme resolu : shared memory

Le `.wasm` compile avec `+atomics` n'avait PAS le flag shared memory.

**Fix** : ajouter ces flags de link :
```
-C link-arg=--shared-memory
-C link-arg=--max-memory=1073741824
-C link-arg=--import-memory
-C link-arg=--export=__wasm_init_tls
-C link-arg=--export=__tls_size
-C link-arg=--export=__tls_align
-C link-arg=--export=__tls_base
```

Commande de build complete :
```bash
RUSTFLAGS='-C target-feature=+atomics,+bulk-memory,+mutable-globals -C link-arg=--shared-memory -C link-arg=--max-memory=1073741824 -C link-arg=--import-memory -C link-arg=--export=__wasm_init_tls -C link-arg=--export=__tls_size -C link-arg=--export=__tls_align -C link-arg=--export=__tls_base' \
cargo +nightly build -p lucivy-wasm --target wasm32-unknown-unknown --release \
  -Z build-std=std,panic_abort
```

Apres : `wasm-bindgen --target web --out-dir bindings/wasm/pkg target/wasm32-unknown-unknown/release/lucivy_wasm.wasm`

Verification : le `.wasm` a bien `flags=0x03` (shared + max) dans la memory section, et le JS glue genere `new WebAssembly.Memory({shared: true, ...})`.

## Probleme resolu : segment_updater ThreadPool

`rayon::ThreadPoolBuilder::new().build()` echoue sur wasm32 car il tente de spawner des OS threads.

**Fix** (`segment_updater.rs`) :
- Les champs `pool` et `merge_thread_pool` sont sous `#[cfg(not(target_arch = "wasm32"))]`
- Les appels `self.pool.spawn(fn)` et `self.merge_thread_pool.spawn(fn)` sont remplaces par `rayon::spawn(fn)` sur wasm32 (utilise le pool global initialise par `initThreadPool`)

**Fix** (`executor.rs`) :
- `Executor::multi_thread()` retourne `Executor::SingleThread` sur wasm32

## Probleme resolu : serveur de test `/pkg/` import

`wasm-bindgen-rayon` fait `import('../../..')` dans `workerHelpers.js` qui resolve vers `/pkg/` (un dossier). Le serveur HTTP de test doit mapper `/pkg/` → `/pkg/lucivy_wasm.js` (le `main` du `package.json`).

**Fix** (`test-playwright.mjs`) : rewrite `/pkg` et `/pkg/` vers `/pkg/lucivy_wasm.js`.

## Probleme actuel : `std::thread::park` non supporte

### Symptome

```
An IO error occurred: 'operation not supported on this platform'
```

L'erreur survient a `commit()`. Le stack trace montre `index_commit` dans le WASM.

### Cause racine

Sur `wasm32-unknown-unknown`, **meme avec `+atomics` et `-Z build-std`**, `std::thread::park()` n'est PAS implemente. Il retourne `io::Error(Unsupported)`.

Toute operation qui bloque via `thread::park` echoue :
- `crossbeam_channel::Receiver::recv()` → utilise `thread::park`
- `crossbeam_channel::Receiver::into_iter()` → appelle `recv()` en boucle
- `oneshot::Receiver::recv()` → utilise `thread::park`

### Pipeline d'indexation (architecture actuelle)

```
add_document() → sender.send(doc) → [channel] → worker_thread: recv() loop → index_documents()
commit() → close channel → worker_thread.join() → schedule_commit().wait()
```

Le worker thread consomme les documents via `crossbeam_channel::into_iter()`, qui bloque avec `recv()`. C'est fondamentalement incompatible avec wasm32.

### Fixes deja appliques (partiels)

1. **`WorkerHandle::join`** (index_writer.rs) : remplace `crossbeam_channel::recv()` par spin-loop `try_recv()` sur wasm32
2. **`FutureResult::wait`** (future_result.rs) : remplace `oneshot::recv()` par spin-loop `try_recv()` sur wasm32
3. **`start_workers`** (index_writer.rs) : skip le spawn de worker threads sur wasm32
4. **`prepare_commit`** (index_writer.rs) : sur wasm32, draine le channel avec `try_iter()` et appelle `index_documents()` inline

### Pourquoi ca ne suffit pas

L'erreur persiste. L'investigation n'est pas terminee. Hypotheses :

1. **`schedule_task` → `rayon::spawn` + `FutureResult::wait()` spin-loop** :
   - `schedule_add_segment().wait()` est appele dans `index_documents()` (ligne 259 de index_writer.rs)
   - Cet appel est maintenant inline sur le thread principal (worker JS)
   - Le `schedule_task` fait `rayon::spawn(fn)` (pool global)
   - Le spin-loop `try_recv` attend le resultat
   - La tache rayon s'execute sur un sous-worker Web Worker
   - **En theorie ca devrait marcher** — le sous-worker peut faire `Atomics.wait`, et `oneshot::send()` ne bloque pas

2. **Autre source de `thread::park`** :
   - Il peut y avoir d'autres appels bloquants dans le code de serialisation de segments, save_metas, garbage_collect
   - `save_metas` appelle `directory.atomic_write()` puis `sync_directory()` — ces deux passent par MemoryDirectory et retournent Ok
   - Mais `garbage_collect_files` dans `schedule_commit` peut impliquer d'autres operations

3. **Le `FutureResult::wait()` spin-loop ne compile pas correctement** :
   - Verifier que le code wasm32 est bien celui attendu (pas de cache)
   - Ajouter un `console.log` via `web_sys` dans le code Rust pour tracer exactement ou l'erreur se produit

4. **Le `rayon::spawn` ne s'execute jamais** :
   - Si le pool rayon n'est pas fonctionnel (sous-workers pas initialises), le spin-loop tourne indefiniment
   - Mais l'erreur n'est PAS un timeout — c'est un `IoError`. Donc le spin-loop n'est peut-etre pas atteint

### Piste la plus probable

L'erreur "operation not supported on this platform" est un `io::Error` qui est **converti** en `LucivyError::IoError`. Il faut tracer exactement **ou** cette conversion se produit. La conversion est a `src/error.rs:117`:
```rust
impl From<io::Error> for LucivyError {
    fn from(io_err: io::Error) -> LucivyError {
        LucivyError::IoError(Arc::new(io_err))
    }
}
```

**Action recommandee** : ajouter un `eprintln!` ou `web_sys::console::log_1` dans cette conversion `From<io::Error>` pour capturer le backtrace et identifier l'appelant.

### Alternative : approche single-thread complete

Si le debugging est trop complexe, une approche radicale :
- Sur wasm32, `schedule_task` execute la closure **inline** (pas de `rayon::spawn`)
- Plus besoin de `FutureResult::wait()` — le resultat est immediat
- Pas de threading du tout sur wasm32 — tout est synchrone
- Le `initThreadPool` n'est plus necessaire (on peut le garder pour d'eventuelles recherches paralleles futures)

Cela revient a faire sur wasm32 :
```rust
fn schedule_task<T, F: FnOnce() -> Result<T>>(&self, task: F) -> FutureResult<T> {
    // Sur wasm32 : executer inline
    let result = task();
    // Retourner un FutureResult deja resolu
    FutureResult::from_result(result)
}
```

## Fichiers modifies dans cette session

| Fichier | Changement |
|---------|-----------|
| `src/indexer/segment_updater.rs` | cfg-guard ThreadPool, rayon::spawn sur wasm32 |
| `src/core/executor.rs` | multi_thread → SingleThread sur wasm32 |
| `src/indexer/index_writer.rs` | WorkerHandle spin-loop, skip workers sur wasm32, inline indexing dans prepare_commit |
| `src/future_result.rs` | FutureResult::wait() spin-loop try_recv sur wasm32 |
| `bindings/wasm/test-playwright.mjs` | rewrite /pkg/ → /pkg/lucivy_wasm.js |
| `bindings/wasm/test-worker-direct.js` | nouveau — test worker minimal |
| `bindings/wasm/test-minimal.html` | nouveau — page de test minimale |

## Test infra

- `initThreadPool(N)` : OK, les sous-workers rayon se chargent
- `Index::create()` (new Index) : OK
- `idx.add()` : OK
- `idx.commit()` : ECHOUE avec IoError
- Le test Playwright (`test-playwright.mjs`) et le test minimal (`test-minimal.html` + `test-worker-direct.js`) reproduisent le bug

## Prochaine etape recommandee

1. Ajouter un log dans `From<io::Error> for LucivyError` pour identifier l'appelant exact
2. OU implémenter l'approche single-thread complete (schedule_task inline + FutureResult::from_result)
3. Tester
4. Si ca marche, nettoyer le code (supprimer WorkerHandle wasm32 devenu inutile, etc.)
