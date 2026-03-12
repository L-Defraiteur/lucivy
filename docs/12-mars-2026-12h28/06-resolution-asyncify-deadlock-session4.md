# Résolution du deadlock ASYNCIFY — session 4

## Problème initial

Après l'import d'un fichier dans le playground WASM, le `lucivy_commit` (et parfois `lucivy_add`) ne retournait jamais vers JavaScript. L'hypothèse initiale était un deadlock de Mutex Rust.

## Découverte clé : ce n'est PAS un deadlock Mutex

Grâce au **ring buffer SharedArrayBuffer** (session 3), on a pu voir que le code Rust s'exécutait entièrement :
```
[add] acquiring writer lock...
[add] writer lock acquired, adding doc...
[add] writer lock released
← JAMAIS de retour vers JS
```

Le Mutex est acquis, utilisé, et libéré normalement. Le problème est dans le **mécanisme de retour ASYNCIFY** vers JavaScript.

## Cause racine : `eprintln!` = yield points ASYNCIFY

Chaque `eprintln!` dans une fonction FFI fait :
1. `fd_write` syscall
2. Proxied vers le main thread (car `PROXY_TO_PTHREAD`)
3. ASYNCIFY yield pour attendre le proxy
4. ASYNCIFY restore pour reprendre

Le problème : **plus il y a de yield points ASYNCIFY dans une même fonction, plus le risque que ASYNCIFY ne puisse pas restaurer la stack correctement** (ASYNCIFY_STACK_SIZE limité à 65536 bytes, stack profonde avec tantivy/lucivy).

### Preuve expérimentale

| Configuration | `lucivy_add` | `lucivy_commit` |
|---|---|---|
| Avec `rlog!` (eprintln) dans add | **BLOQUÉ** — ne retourne pas | jamais atteint |
| Sans `rlog!` dans add | **OK** — retourne | **BLOQUÉ** — ne retourne pas |
| Sans `rlog!`, commit avec `ring_write` seulement | add OK | commit atteint `writer.commit()` mais **BLOQUÉ** dans le code interne tantivy |

Conclusion : le problème n'est pas notre code, c'est que **`writer.commit()` fait trop d'opérations bloquantes internes** (Mutex, Condvar, channels flume) pour qu'ASYNCIFY puisse les gérer.

## Solution : commit sur pthread dédié

Au lieu de faire passer `writer.commit()` par ASYNCIFY (proxy pthread → ccall → ASYNCIFY yield/restore), on le fait tourner sur un **vrai pthread séparé** :

### Côté Rust (`bindings/emscripten/src/lib.rs`)

```rust
static COMMIT_STATUS: AtomicU32 = AtomicU32::new(0);
// 0=idle, 1=running, 2=done_ok, 3=done_error

pub extern "C" fn lucivy_commit_status_ptr() -> *const u32 { ... }

pub unsafe extern "C" fn lucivy_commit_async(ctx) -> i32 {
    COMMIT_STATUS.store(1, ...);
    std::thread::spawn(move || {
        // Tout le commit ici — aucun ASYNCIFY
        writer.lock() → writer.commit() → reader.reload()
        COMMIT_STATUS.store(2, ...); // ou 3 si erreur
    });
    0 // retourne immédiatement
}

pub extern "C" fn lucivy_commit_finish() -> *const c_char { ... }
```

### Côté JS (`playground/js/lucivy-worker.js`)

```javascript
// 1. Spawn le thread (retour immédiat via ASYNCIFY, trivial)
await Module.ccall('lucivy_commit_async', 'number', ['number'], [ctx], { async: true });

// 2. Poll via SharedArrayBuffer — ZÉRO ccall
const statusView = new Int32Array(sab, statusPtr, 1);
const poll = setInterval(() => {
    const status = Atomics.load(statusView, 0);
    if (status >= 2) { ... } // done
}, 50);

// 3. Reset status
await Module.ccall('lucivy_commit_finish', ...);
```

### Pourquoi ça marche

- Le pthread dédié a sa propre stack (2MB via `STACK_SIZE`), pas de limitation ASYNCIFY
- Les Mutex/Condvar/futex fonctionnent normalement entre pthreads natifs
- Le proxy pthread reste libre pendant le commit
- Le polling SAB ne fait aucun ccall → aucune interaction ASYNCIFY

## Autres changements dans cette session

### 1. Ring buffer SAB pour l'observabilité

`ring_write(msg)` écrit directement dans un buffer 64KB en mémoire linéaire WASM. Le main thread JS lit via `Atomics.load()` toutes les 50ms. Fonctionne même pendant les deadlocks car **aucun ccall nécessaire**.

### 2. `rlog!` nettoyé dans les fonctions FFI

- `lucivy_add` : tous les `rlog!` supprimés (évite les yield points ASYNCIFY parasites)
- `lucivy_commit` : remplacé par la solution pthread dédiée
- Seul `main()` garde un `rlog!` (au démarrage, avant toute opération critique)

### 3. Scheduler LOG_HOOK

`set_scheduler_log_hook()` dans `scheduler.rs` route les events `[sched]` vers `ring_write()`, visibles via `[rust] [sched] ...` dans la console du main thread.

### 4. `serve.mjs` avec headers natifs

Headers `Cross-Origin-Opener-Policy`, `Cross-Origin-Embedder-Policy`, `Cache-Control: no-store` — plus besoin de coi-serviceworker qui cachait le WASM.

### 5. ZIP central directory

Le parseur ZIP lisait les tailles depuis le local file header. Avec des zips qui utilisent des data descriptors (flag bit 3, courant avec `zip` Linux), les tailles du local header sont à 0. Réécrit pour lire le **central directory** qui a toujours les vraies tailles.

## Fichiers modifiés

```
bindings/emscripten/src/lib.rs     # Ring buffer, commit_async, commit_status_ptr, commit_finish
bindings/emscripten/build.sh       # Exports mis à jour
playground/js/lucivy-worker.js     # SAB ring init, commit status ptr, poll-based commit
playground/js/lucivy.js            # _startLogRingPoller avec Atomics
playground/index.html              # extractZip via central directory
playground/serve.mjs               # COOP/COEP natifs
src/actor/scheduler.rs             # LOG_HOOK
src/lib.rs                         # pub use set_scheduler_log_hook
```

## Leçons apprises

1. **ASYNCIFY a des limites** : ne pas faire passer d'opérations bloquantes profondes par ASYNCIFY. Utiliser des pthreads dédiés + signalisation atomique.

2. **`eprintln!` dans FFI = poison pour ASYNCIFY** : chaque `fd_write` ajoute un yield point. Dans une fonction avec beaucoup de logique, ça dépasse la capacité de restauration de stack ASYNCIFY.

3. **SharedArrayBuffer est le canal de communication idéal** en WASM multithreadé : lecture directe par le main thread, aucune dépendance sur le proxy pthread, fonctionne même pendant les deadlocks.

4. **Le central directory ZIP est la seule source fiable** pour les tailles de fichiers compressés.
