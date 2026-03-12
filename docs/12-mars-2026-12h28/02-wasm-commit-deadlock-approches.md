# WASM commit deadlock — analyse et approches

## Le problème

En emscripten avec pthreads, `writer.commit()` deadlock systématiquement.

**Chaîne de causalité :**
1. `writer.commit()` envoie des messages aux acteurs (indexeurs, merge) via le scheduler
2. Les acteurs tournent sur des pthreads emscripten
3. Les pthreads ont besoin de l'event loop du worker pour être schedulés/coordonnés
4. Mais le thread qui appelle `commit()` (que ce soit le worker thread ou un thread séparé) attend la réponse des acteurs via `reply.wait_blocking()` → Condvar/futex
5. Les acteurs attendent l'event loop, le commit attend les acteurs → **deadlock**

**Ce qui marche :** lecture seule (importSnapshot, search, numDocs) — pas de commit, pas d'acteurs write.

**Ce qui ne marche pas :** tout write path passant par `writer.commit()`.

**Observation clé :** en testant étape par étape via la console Chrome (chaque commande séparée par une pause humaine), le commit passe en 42ms. Le problème apparaît uniquement quand les appels sont chaînés programmatiquement (l'event loop n'a jamais l'occasion de tourner entre les opérations).

## Ce qu'on a essayé (et pourquoi ça échoue)

### 1. Commit synchrone + ASYNCIFY
ASYNCIFY permet aux appels Rust de yield à l'event loop pendant un `Mutex::lock()` ou `Condvar::wait()`. Mais le commit descend trop profond : crossbeam channels → futex_wait. ASYNCIFY ne peut pas intercepter les futex système.

**Résultat :** deadlock quand chaîné, marche en interactif (les pauses humaines libèrent l'event loop).

### 2. Remplacement crossbeam → Mutex+Condvar dans reply.rs
On a remplacé le oneshot crossbeam (`bounded(1)`) par un `Arc<Mutex<State<T>> + Condvar>` dans `reply.rs`. ASYNCIFY intercepte bien `Condvar::wait()` standard.

**Résultat :** les 1142 tests Rust passent. Mais en WASM, même deadlock — le problème n'est pas que dans reply.rs. Le `writer.commit()` passe par d'autres chemins bloquants (crossbeam channels des mailboxes acteurs, rayon thread pool, etc.).

### 3. Thread séparé + poll (commit_poll)
Le commit tourne sur un `std::thread::spawn` dédié. Le JS poll `lucivy_commit_poll` toutes les 5ms via `setTimeout`, ce qui yield l'event loop entre chaque poll.

**Résultat :** le thread commit lui-même deadlock car il attend les acteurs pthreads qui ont besoin de l'event loop. Le fait que le JS yield ne suffit pas — c'est le thread Rust commit qui est bloqué, pas le JS.

### 4. ccall sans `{ async: true }` (bypass ASYNCIFY)
Appeler `Module._lucivy_commit(ctx)` directement au lieu de `ccall(..., { async: true })`.

**Résultat :** `func is not a function` — quand ASYNCIFY est activé, emscripten wrappe toutes les fonctions exportées et le ccall synchrone ne trouve pas les bonnes signatures.

## Approches envisagées

### Approche A : Callback emscripten (thread commit → notification JS)

**Principe :** le thread commit, une fois terminé, dispatche un callback vers le worker thread via `emscripten_async_run_in_main_runtime_thread`. Le JS n'a pas besoin de poll — il reçoit une notification directe.

**Implémentation :**
```rust
// Dans le thread commit, une fois fini :
extern "C" {
    fn emscripten_async_run_in_main_runtime_thread(
        sig: i32, func: extern "C" fn(*mut c_void), arg: *mut c_void
    );
}

extern "C" fn commit_done_callback(ctx: *mut c_void) {
    // Côté main thread : post un message JS avec le résultat
    // via EM_ASM ou une fonction exportée
}
```

```js
// Côté worker : enregistre une callback
Module['onCommitDone'] = (ctx, errorPtr) => {
    // resolve/reject la promise du commit
};
```

**Avantages :**
- Zéro polling, notification instantanée
- Le thread commit tourne librement, le worker thread reste libre pour l'event loop

**Inconvénients :**
- Plumbing emscripten non trivial (FFI callback cross-thread)
- Ne résout pas le deadlock fondamental si le thread commit attend lui-même des pthreads
- Complexité de gestion d'erreur (le callback doit transmettre les erreurs)

**Résout le deadlock ?** NON — même problème que l'approche 3. Le thread commit est lui-même bloqué en attendant les acteurs.

### Approche B : Se passer de crossbeam entièrement

**Principe :** remplacer toutes les utilisations de crossbeam par des primitives std (`Mutex+Condvar`, `mpsc`) ou une lib WASM-friendly comme `flume`.

**Usages actuels de crossbeam :**
- `actor/mailbox.rs` — bounded channels pour les mailboxes acteurs (chemin chaud)
- `actor/reply.rs` — oneshot pour les réponses (**déjà remplacé** par Mutex+Condvar)
- `actor/events.rs` — channels pour l'observabilité
- `core/executor.rs` — unbounded channel pour collecter les résultats du thread pool search
- `reader/warming.rs` — `crossbeam_channel::tick()` pour le GC timer
- `directory/file_watcher.rs` — pas utilisé en WASM

**Complexité :** ~2h. `flume` est un drop-in replacement. `std::sync::mpsc::Sender` n'est pas `Sync` donc ne compile pas pour les mailboxes multi-producer → `flume` ou wrapper custom.

**Résout le deadlock ?** PARTIELLEMENT — `flume` utilise `Mutex<VecDeque>` au lieu de futex, donc ASYNCIFY peut intercepter. Mais le scheduler spawn des threads via `std::thread::spawn` qui passe par `pthread_create` emscripten → besoin de l'event loop. Le problème est plus profond que juste les channels.

### Approche C : Mode commit coopératif pour WASM

**Principe :** au lieu de `reply.wait_blocking()` (qui bloque le thread), utiliser `reply.wait_cooperative()` qui pompe le scheduler entre chaque tentative de réception. En WASM, le commit ne bloque jamais — il boucle en faisant avancer les acteurs lui-même.

**`wait_cooperative` existe déjà dans reply.rs :**
```rust
pub fn wait_cooperative<F>(self, mut run_step: F) -> T
where F: FnMut() -> bool {
    loop {
        if let Some(value) = self.try_recv() { return value; }
        if !run_step() {
            // wait_timeout 1ms puis retry
        }
    }
}
```

**Implémentation :**
1. Ajouter un flag `cooperative: bool` au `Scheduler` (ou un mode `SchedulerMode::Cooperative`)
2. Quand cooperative=true, tous les `wait_blocking()` dans le write path deviennent `wait_cooperative(|| scheduler.run_one_step())`
3. Le commit tourne en single-thread effectif — il fait avancer les acteurs dans sa propre boucle
4. Côté emscripten : `LucivyHandle::create()` passe `cooperative: true`

**Avantages :**
- Résout le deadlock à la racine — pas besoin de pthreads pour les acteurs
- Pas de polling JS, pas de thread séparé, commit synchrone propre
- Compatible ASYNCIFY (les `try_recv` et `run_one_step` ne bloquent jamais longtemps)
- Le scheduler a déjà `run_one_step()` et `wait_cooperative()`

**Inconvénients :**
- Le commit est effectivement single-threaded (pas de parallélisme merge)
- Faut propager le mode coopératif dans toute la chaîne commit → prepare_commit → acteurs
- Risque de régression si un chemin oublie de vérifier le mode

**Résout le deadlock ?** OUI — c'est la seule approche qui élimine la dépendance aux pthreads pour le write path.

## Recommandation

**Court terme :** le playground fonctionne en lecture (snapshot import + search). Le write path (import fichier utilisateur) ne marche pas.

**Moyen terme :** **Approche C** (commit coopératif). C'est la seule qui résout le problème à la racine. Le scheduler a déjà les primitives nécessaires (`run_one_step`, `wait_cooperative`). L'effort est de propager un mode coopératif dans le write path.

**Ce qui est déjà fait :**
- `reply.rs` remplacé par Mutex+Condvar (bon pour natif, prérequis pour C)
- `wait_cooperative` implémenté et testé
- `run_one_step` existe dans le scheduler

**Ce qui reste :**
- Flag `cooperative` dans le scheduler
- Propager le mode dans `IndexWriter::commit()` → `prepare_commit()` → acteurs
- Tester en WASM
