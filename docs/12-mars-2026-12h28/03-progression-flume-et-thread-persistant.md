# Progression : flume + thread commit persistant

## Résumé

Session de travail sur la branche `wasm-flume` (créée depuis `scheduler-beta` après commit des travaux précédents).

## Ce qui a été fait

### 1. Remplacement crossbeam → flume (complet)

**Branche** : `wasm-flume`
**Tous les usages de crossbeam-channel remplacés par flume dans le code source :**

| Fichier | Avant | Après |
|---|---|---|
| `Cargo.toml` | `crossbeam-channel = "0.5.12"` | `flume = "0.11"` |
| `src/actor/mailbox.rs` | `use crossbeam_channel as channel` | `use flume as channel` |
| `src/actor/events.rs` | `use crossbeam_channel as channel` | `use flume as channel` |
| `src/core/executor.rs` | `crossbeam_channel::unbounded/bounded` | `flume::unbounded/bounded` |
| `src/reader/warming.rs` | `crossbeam_channel::tick(GC_INTERVAL)` | `loop { std::thread::sleep(GC_INTERVAL); ... }` |
| `src/directory/mmap_directory/file_watcher.rs` | `crossbeam_channel::unbounded` | `flume::unbounded` |
| `src/directory/tests.rs` | `crossbeam_channel::unbounded` | `flume::unbounded` |
| `src/core/tests.rs` | `crossbeam_channel::unbounded` | `flume::unbounded` |

**Résultat** : 1142 tests passent, 0 échecs. Drop-in replacement quasi parfait.

**Pourquoi flume** : flume utilise `Mutex<VecDeque> + Condvar` au lieu de futex. ASYNCIFY (emscripten) peut intercepter les `Condvar::wait()` standard mais pas les futex système utilisés par crossbeam.

### 2. Diagnostic du deadlock WASM commit

**Flume seul ne résout pas le deadlock.** Le commit deadlock persiste même avec flume.

**Diagnostic précis obtenu avec logs relayés worker→main thread :**

```
[commit] yielding event loop...
[commit] calling lucivy_commit...
← BLOQUE ICI — lucivy_commit ne retourne jamais
```

**Cause racine identifiée** : `Module.ccall('lucivy_commit', ..., { async: true })` ne retourne jamais.

Chaîne de causalité :
1. Worker JS fait `await Module.ccall('lucivy_commit', ..., { async: true })`
2. ASYNCIFY suspend le worker JS event loop pendant l'appel
3. ccall route vers le proxy pthread (PROXY_TO_PTHREAD)
4. `lucivy_commit()` fait `std::thread::spawn()` → `pthread_create`
5. `pthread_create` demande au main browser thread de créer un Web Worker
6. Mais l'event loop du worker est suspendue (étape 2) → impossible de créer le Worker
7. **Deadlock** : spawn attend l'event loop, event loop attend le ccall, ccall attend le spawn

**Note** : le problème n'est PAS l'exhaustion du pool pthread. Même avec `LUCIVY_SCHEDULER_THREADS=4` et `PTHREAD_POOL_SIZE=8` (4 slots libres), le deadlock persiste. C'est le mécanisme `pthread_create` sous ASYNCIFY qui est fondamentalement incompatible avec un spawn pendant un ccall.

### 3. Solution : thread commit persistant

**Principe** : ne jamais faire `pthread_create` pendant un appel FFI. Le thread commit est créé à l'init (dans `lucivy_create` / `lucivy_open_finish`), quand l'event loop est libre.

**Implémentation** (`bindings/emscripten/src/lib.rs`) :

```rust
struct CommitWorker {
    sender: flume::Sender<CommitJob>,       // envoie un job au thread
    result: Arc<Mutex<Option<Result<(), String>>>>, // résultat du dernier commit
    busy: Arc<AtomicBool>,                  // true pendant le commit
    _thread: JoinHandle<()>,                // thread persistant
}
```

- `CommitWorker::new()` spawne le thread une seule fois
- Le thread fait `receiver.recv()` en boucle (flume channel bounded(1))
- `lucivy_commit()` envoie un `CommitJob` via `try_send()` — zéro `pthread_create`
- `lucivy_commit_poll()` check `busy` + `result`

**Aussi ajouté** :
- `lucivy_configure(scheduler_threads, thread_pool_size)` — API FFI pour configurer le nombre de threads scheduler avant le premier usage (avec warning si pool saturé)
- `LUCIVY_SCHEDULER_THREADS` env var dans le scheduler pour override le auto-detect
- Default dans `main()` emscripten : scheduler=4 threads, debug=on
- Relay de logs worker→main thread via `postMessage({ type: 'log', msg })` + capture dans `lucivy.js`

### 4. Aussi ajouté : flume dans bindings/emscripten/Cargo.toml

Le crate emscripten utilise `flume::bounded` et `flume::Sender` pour le CommitWorker.

## État actuel

- **Build WASM** : OK (compilé avec succès)
- **Test** : pas encore fait — le build vient de finir
- **Hypothèse** : le thread commit persistant devrait résoudre le deadlock car il élimine le `pthread_create` pendant le ccall

## Fichiers modifiés (par rapport à scheduler-beta)

```
Cargo.toml                           # crossbeam → flume
src/actor/mailbox.rs                 # crossbeam → flume
src/actor/events.rs                  # crossbeam → flume
src/actor/scheduler.rs               # LUCIVY_SCHEDULER_THREADS env var
src/core/executor.rs                 # crossbeam → flume
src/reader/warming.rs                # tick() → sleep loop
src/directory/mmap_directory/file_watcher.rs  # crossbeam → flume
src/directory/tests.rs               # crossbeam → flume
src/core/tests.rs                    # crossbeam → flume
bindings/emscripten/Cargo.toml       # ajout flume dep
bindings/emscripten/src/lib.rs       # CommitWorker persistant + lucivy_configure
bindings/emscripten/build.sh         # ajout _lucivy_configure export
playground/js/lucivy-worker.js       # logs commit + wlog relay
playground/js/lucivy.js              # worker log relay handler
```

## BUG CRITIQUE : le playground servait un ancien WASM

**Symptôme** : tous nos tests WASM (flume, thread persistant, scheduler 4 threads) donnaient exactement le même comportement — deadlock au commit. Aucun changement Rust n'avait d'effet.

**Cause** : `playground/pkg/` contenait une **copie statique** du build du 9 mars (4.1MB), pas le build courant (6.9MB). Le `build.sh` output dans `bindings/emscripten/pkg/`, mais le serveur HTTP sert `playground/pkg/`. Les deux dossiers étaient indépendants.

```
bindings/emscripten/pkg/lucivy.wasm  → 6.9MB (build du jour, avec flume + commit persistant)
playground/pkg/lucivy.wasm           → 4.1MB (copie du 9 mars, avec crossbeam + spawn)
```

Vérification :
```bash
curl -s http://localhost:8787/pkg/lucivy.wasm | wc -c   # 4190584 (ANCIEN)
wc -c bindings/emscripten/pkg/lucivy.wasm                # 6897905 (COURANT)
```

**Conséquence** : on a passé des heures à diagnostiquer un deadlock qui était causé par du code qu'on avait déjà corrigé. Le diagnostic (chaîne ASYNCIFY → proxy pthread → pthread_create) est valide pour l'ancien code, mais on n'a jamais testé nos corrections.

**Fix temporaire** : symlink `playground/pkg → ../bindings/emscripten/pkg`

**Fix permanent nécessaire** : modifier `build.sh` pour copier/outputter directement dans `playground/pkg/`, ou mieux, que le build script vérifie que le playground pointe vers le bon binaire. Un check de cohérence en fin de build serait idéal :

```bash
# En fin de build.sh — vérifier que le playground sert le bon build
PLAYGROUND_PKG="$SCRIPT_DIR/../../playground/pkg"
if [ -d "$PLAYGROUND_PKG" ] && [ ! -L "$PLAYGROUND_PKG" ]; then
    echo "WARNING: playground/pkg/ is a copy, not a symlink!"
    echo "Run: rm -rf playground/pkg && ln -s ../bindings/emscripten/pkg playground/pkg"
fi
```

## Prochaine étape

Relancer les tests avec le bon WASM. Si le thread persistant résout le deadlock, on a notre solution. Si le deadlock persiste, on aura un vrai diagnostic (cette fois avec le bon binaire).
