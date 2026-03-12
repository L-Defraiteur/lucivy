# Observabilité SAB Ring Buffer — session 3

## Objectif de la session

Résoudre le manque total d'observabilité dans le WASM pendant les deadlocks. Deux axes :
1. **Ring buffer SharedArrayBuffer** : logs Rust lus directement par le main thread JS sans ccall
2. **Serveur HTTP propre** : `serve.mjs` avec headers COOP/COEP natifs, plus de coi-serviceworker cache

## Ce qui a été implémenté

### 1. Ring buffer SAB (SharedArrayBuffer)

**Principe** : buffer statique de 64KB dans la mémoire linéaire WASM (qui EST un SharedArrayBuffer quand pthreads sont activés). Rust écrit dedans, le main thread JS lit directement via `Atomics.load()` — **zéro ccall, zéro proxy pthread**.

**Layout du buffer** :
```
[0..4]   write_pos   (AtomicU32) — prochain offset d'écriture
[4..8]   wrap_count  (AtomicU32) — incrémenté à chaque wrap-around
[8..]    entrées, chaque: [u16_le longueur][utf8 bytes...]
```

**Fichiers modifiés** :

- **`bindings/emscripten/src/lib.rs`** :
  - Ajout `LogRing` struct avec `UnsafeCell<[u8; 65536]>`, `#[repr(C, align(4))]`
  - `ring_write(msg)` : écrit dans le buffer avec `RING_LOCK` (Mutex) pour sérialiser les écritures
  - `ring_write_pos()` / `ring_wrap_count()` : accès atomique aux headers
  - `rlog()` modifié : appelle `ring_write()` EN PLUS de `LOG_BUF.push()` et `eprintln!()`
  - Exports FFI : `lucivy_log_ring_ptr()` → pointeur, `lucivy_log_ring_size()` → 65536
  - `lucivy_add` : ajout de `rlog!` avant/après writer.lock() + `drop(writer)` explicite
  - `lucivy_commit` : ajout `try_lock()` diagnostic avant le `lock()` bloquant

- **`bindings/emscripten/build.sh`** :
  - Ajout `_lucivy_log_ring_ptr` et `_lucivy_log_ring_size` dans EXPORTED_FUNCTIONS

- **`playground/js/lucivy-worker.js`** :
  - Après init, récupère `ringPtr` et `ringSize` via ccall (une seule fois)
  - Envoie `{ type: 'logRing', buffer: Module.HEAPU8.buffer, ringPtr, ringSize }` au main thread

- **`playground/js/lucivy.js`** :
  - Reçoit le message `logRing`
  - `_startLogRingPoller(sab, ringPtr, ringSize)` : `setInterval(50ms)` qui lit les nouvelles entrées via `Atomics.load()` sur les Int32Array views et `console.log('[rust]', msg)`

### 2. Serveur HTTP avec headers natifs

**`playground/serve.mjs`** reécrit :
- `Cross-Origin-Opener-Policy: same-origin`
- `Cross-Origin-Embedder-Policy: require-corp`
- `Cross-Origin-Resource-Policy: same-origin`
- `Cache-Control: no-store, no-cache, must-revalidate` — **plus jamais de WASM stale**
- Port configurable : `node serve.mjs 8787`
- Protection traversal de répertoire
- Types MIME pour `.mjs`, `.zip`

### 3. Hook scheduler events → ring buffer

**Problème** : les events `[sched]` (ThreadParked, ActorSpawned, etc.) passaient uniquement par `eprintln!` → proxy pthread → main thread. Pendant un deadlock du proxy, on les perdait.

**Solution** :
- **`src/actor/scheduler.rs`** : ajout `static LOG_HOOK: Mutex<Option<Box<dyn Fn(&str) + Send>>>` + `pub fn set_scheduler_log_hook()`
- Le debug thread appelle le hook en plus de `writeln!(stderr)`
- **`src/lib.rs`** : `pub use actor::scheduler::set_scheduler_log_hook;`
- **`bindings/emscripten/src/lib.rs`** dans `main()` : `ld_lucivy::set_scheduler_log_hook(|msg| ring_write(msg));`

## Résultats des tests

### Test 1 : Ring buffer au démarrage — **FONCTIONNE**

```
[init] log ring: offset=691252, size=65536
[rust] [lucivy-wasm] main() started, default scheduler_threads=4, debug=on
```

Les logs Rust apparaissent dans la console du main thread via `[rust]` prefix. Les events scheduler via `[sched]` visibles via eprintln (proxy mechanism).

### Test 2 : Import fichier + commit — **DIAGNOSTIC OBTENU**

Séquence observée :
```
[rust] [add] acquiring writer lock...
[rust] [add] writer lock acquired, adding doc...
[rust] [add] writer lock released
← BLOQUÉ ICI — lucivy_add ne retourne jamais vers JS
```

**Découverte critique** : `lucivy_add` a **complètement terminé côté Rust** (lock acquis → doc ajouté → lock relâché → rlog écrit au ring buffer), mais le **ccall ASYNCIFY n'a jamais retourné la valeur vers JavaScript**.

Le `[callStr] lucivy_add returned` n'apparaît jamais dans les logs worker.

### Test 3 : Scheduler events via ring buffer — **EN COURS**

Build fait avec le hook scheduler. Pas encore testé au moment de l'écriture de ce rapport.

## Analyse du bug

### Ce n'est PAS un deadlock de Mutex

Contrairement à l'hypothèse initiale (session 2), le writer lock n'est pas le problème. Le cycle complet acquire→use→release est visible dans le ring buffer. Le lock est bien libéré.

### C'est un bug ASYNCIFY

Le problème est dans le mécanisme de retour ASYNCIFY. Quand une fonction FFI contient un appel qui trigger un yield ASYNCIFY (comme `Mutex::lock()` qui utilise futex), la séquence est :

1. `lucivy_add` appelé sur le proxy pthread via ccall `{async: true}`
2. `Mutex::lock()` → futex → ASYNCIFY yield (même si non-contesté, le futex peut yield)
3. ASYNCIFY sauvegarde la stack, retourne au event loop
4. Event loop traite le futex (prêt immédiatement)
5. ASYNCIFY restaure la stack, `lock()` retourne
6. Le reste de la fonction s'exécute normalement (add_document, drop, rlog)
7. **La fonction retourne sa valeur...**
8. **...mais ASYNCIFY ne complète jamais le retour vers JS**

Hypothèses sur le blocage au retour :
- `ASYNCIFY_STACK_SIZE=65536` potentiellement insuffisant pour la profondeur de stack
- Le mécanisme de retour ASYNCIFY a besoin du proxy pthread pour finaliser, mais le proxy est dans un état incohérent
- Les `rlog!()` ajoutés (qui font `eprintln!` → `fd_write` → proxy vers main thread) ajoutent des points de yield ASYNCIFY supplémentaires qui perturbent le déroulement

### Note importante

Dans la session 2, `lucivy_add` retournait normalement (sans les rlog supplémentaires). C'est le `lucivy_commit` qui deadlockait sur `writer.lock()`. L'ajout des rlog dans `lucivy_add` a changé le comportement — ça bloque maintenant plus tôt, dans le retour ASYNCIFY de `lucivy_add` au lieu du commit.

Cela confirme que le problème est lié à **ASYNCIFY + nombre/position des yield points**, pas au code logique Rust.

## Fichiers modifiés (cette session)

```
bindings/emscripten/src/lib.rs    # Ring buffer, rlog→ring_write, exports FFI, try_lock diagnostic, drop explicite
bindings/emscripten/build.sh      # +_lucivy_log_ring_ptr, +_lucivy_log_ring_size exports
playground/js/lucivy-worker.js    # Envoi SAB info au main thread
playground/js/lucivy.js           # _startLogRingPoller avec Atomics
playground/serve.mjs              # Reécrit: COOP/COEP/CORP natifs, no-store cache
playground/index.html             # Commentaire coi-serviceworker
src/actor/scheduler.rs            # LOG_HOOK + set_scheduler_log_hook()
src/lib.rs                        # pub use set_scheduler_log_hook
```

## Prochaines pistes

1. **Tester les scheduler events via ring buffer** : recharger la page avec le nouveau build et voir si les events `[sched]` apparaissent via `[rust]` pendant le blocage

2. **Enlever les rlog dans lucivy_add** : revenir au code minimal (sans rlog/drop explicite dans add) pour confirmer que c'est bien les yield points supplémentaires qui causent le blocage précoce. Garder uniquement le try_lock dans commit.

3. **Augmenter ASYNCIFY_STACK_SIZE** : passer de 65536 à 131072 ou 262144 dans build.sh pour voir si le problème est un stack overflow ASYNCIFY silencieux

4. **Tester sans ASYNCIFY yield dans lucivy_add** : remplacer `Mutex::lock()` par `try_lock()` dans lucivy_add (pas de futex, donc pas de yield ASYNCIFY) pour confirmer l'hypothèse

5. **Alternative : commit sur un pthread dédié sans ASYNCIFY** : spawner un thread qui fait le commit, et utiliser le ring buffer SAB pour signaler la complétion (flag atomique lu par JS via polling, zéro ccall)

6. **Investiguer le retour ASYNCIFY** : ajouter des logs emscripten (`-sASSERTIONS=2 -sASYNCIFY_DEBUG`) pour voir exactement où ASYNCIFY coince au retour
