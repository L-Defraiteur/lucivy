# Plan — Luciole async executor

## Vision

Ajouter un executor de futures dans luciole, intégré au scheduler
d'actors existant. Pas un "mini tokio" — une alternative qui exploite
les spécificités de luciole (priorités, actors, pool persistant).

## Ce que luciole a déjà

- **Pool de threads persistant** — N threads créés au démarrage, jamais détruits
- **Scheduler à priorités** — BinaryHeap, 5 niveaux (Idle → Critical)
- **Actors** — `handle(msg)` pour travail déclenché, `poll_idle()` pour polling
- **Batch processing** — jusqu'à 1024 messages traités par turn d'actor
- **WakeHandle** — `AtomicBool` + `Condvar` pour réveiller un actor idle
- **Cooperative mode** — `run_one_step()` pour WASM single-thread
- **Drain** — synchronisation : attendre que tous les messages soient traités

## Ce qu'on ajoute

### 1. FutureTask — wrapper autour d'un Future

```rust
struct FutureTask<T> {
    future: Pin<Box<dyn Future<Output = T> + Send>>,
    state: TaskState, // Pending | Ready(T) | Cancelled
}

enum TaskState<T> {
    Pending,
    Ready(T),
    Cancelled,
}
```

### 2. FutureSlot — stockage dans l'actor

```rust
struct FutureSlot {
    tasks: Vec<(TaskId, Box<dyn PollableTask>)>,
    next_id: u64,
    waker: LucioleWaker, // réveille l'actor quand un future est prêt
}

trait PollableTask: Send {
    /// Poll the future. Returns true if completed.
    fn poll(&mut self, cx: &mut Context<'_>) -> bool;
}
```

### 3. LucioleWaker — le Waker intégré au scheduler

Le `Waker` standard Rust a besoin d'un pointeur `RawWaker`. Notre implémentation :

```rust
struct LucioleWaker {
    wake_handle: WakeHandle, // réutilise le WakeHandle existant de l'actor
}

impl Wake for LucioleWaker {
    fn wake(self: Arc<Self>) {
        // Marque l'actor comme non-idle → le scheduler le re-poll
        self.wake_handle.wake();
    }
}
```

Quand un future fait `waker.wake()`, ça réveille l'actor dans le pool
luciole. Le scheduler re-appelle `poll_idle()`, qui re-poll le future.

C'est la pièce centrale : le Waker de std::future redirige vers le
scheduler luciole, pas vers un runtime séparé.

### 4. AsyncScope — API publique

```rust
/// Scope pour soumettre des futures à un actor.
pub struct AsyncScope {
    sender: ActorRef<AsyncMsg>,
}

impl AsyncScope {
    /// Soumet un future. Retourne un handle pour récupérer le résultat.
    pub fn spawn<F, T>(&self, future: F) -> FutureHandle<T>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let (tx, rx) = oneshot();
        self.sender.send(AsyncMsg::Spawn {
            task: Box::new(FutureTaskWrapper { future, result_tx: tx }),
        });
        FutureHandle { rx }
    }

    /// Soumet un future sans attendre le résultat.
    pub fn spawn_detached<F>(&self, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.sender.send(AsyncMsg::SpawnDetached {
            task: Box::new(DetachedTask { future }),
        });
    }
}

/// Handle pour attendre le résultat d'un future.
pub struct FutureHandle<T> {
    rx: OneshotReceiver<T>,
}

impl<T> FutureHandle<T> {
    /// Attente coopérative (ne bloque pas le scheduler).
    pub fn wait(self) -> T {
        self.rx.wait_cooperative()
    }

    /// Check sans bloquer.
    pub fn try_get(&mut self) -> Option<T> {
        self.rx.try_recv()
    }
}
```

### 5. AsyncActor — l'actor qui drive les futures

```rust
struct AsyncActor {
    slots: FutureSlot,
    priority: Priority,
}

impl Actor for AsyncActor {
    type Msg = AsyncMsg;

    fn handle(&mut self, msg: AsyncMsg) -> ActorStatus {
        match msg {
            AsyncMsg::Spawn { task } => self.slots.push(task),
            AsyncMsg::SpawnDetached { task } => self.slots.push(task),
        }
        ActorStatus::Alive
    }

    fn poll_idle(&mut self) -> Poll<()> {
        if self.slots.is_empty() {
            return Poll::Pending; // rien à faire, dormir
        }

        let waker = self.slots.waker.clone().into_waker();
        let mut cx = Context::from_waker(&waker);

        // Poll tous les futures pending
        self.slots.tasks.retain_mut(|(_, task)| {
            !task.poll(&mut cx) // retirer les complétés
        });

        if self.slots.tasks.is_empty() {
            Poll::Pending // plus de travail
        } else {
            Poll::Ready(()) // encore du travail, re-scheduler
        }
    }

    fn priority(&self) -> Priority {
        self.priority
    }
}
```

### 6. Intégration avec le scheduler existant

Aucun changement au scheduler. Le mécanisme est entièrement dans l'actor :

```
Message arrive → handle() stocke le future
                → poll_idle() poll les futures
                → Waker.wake() → WakeHandle.wake() → scheduler re-poll
```

Le scheduler voit un actor normal. Il ne sait pas qu'il y a des futures
dedans. C'est transparent.

### 7. Priorités async

Contrairement à tokio (toutes les tasks ont la même priorité), luciole
permet des niveaux :

```rust
// Async OPFS (ne bloque pas la recherche)
let opfs_scope = AsyncScope::with_priority(Priority::Idle);
opfs_scope.spawn_detached(async { opfs_write(...).await });

// Async search aggregation (haute priorité)
let search_scope = AsyncScope::with_priority(Priority::High);
let result = search_scope.spawn(async { aggregate_results(...).await });
```

Plusieurs `AsyncActor` peuvent coexister dans le pool, chacun avec sa
priorité. Le scheduler les schedule naturellement selon leurs priorités.

## Usage concret : OPFS

```rust
// Au boot
let opfs = AsyncScope::with_priority(Priority::Idle);

// Après commit
opfs.spawn_detached(async move {
    for (name, data) in changed_files {
        opfs_write_sync(&name, &data); // sync access handle (bloquant mais court)
    }
});

// Chargement initial
let files = opfs.spawn(async {
    opfs_read_all("/index/shard_0/").await
}).wait();
```

## Usage concret : LUCIDS delta sync

```rust
let sync_scope = AsyncScope::with_priority(Priority::Low);

sync_scope.spawn_detached(async move {
    let delta_blob = fetch(url).await;  // réseau
    let delta = deserialize_sharded_delta(&delta_blob)?;
    handle.apply_delta(delta)?;         // applique segments
    opfs_sync_changed_files().await;    // persiste
});
```

## Ce qu'on n'a PAS (et pourquoi c'est OK)

### IO driver (epoll/kqueue/iocp)
Surveille des milliers de sockets réseau pour dire "le socket #N est
prêt". Nécessaire pour un serveur HTTP avec 10K connexions simultanées.
On n'en a pas besoin car on fait du file I/O local, pas du réseau massif.

**Si on en avait besoin** : ajouter le crate `mio` (~5K lignes) comme
IO driver. Le `LucioleWaker` appellerait `mio::Waker` pour réveiller
le reactor. Faisable, juste pas prioritaire.

### Timer wheel
Structure pour gérer des milliers de timers simultanés (O(1) insert/expire).
On peut faire des timers simples avec `poll_idle()` + `Instant::elapsed()`.
Pas optimal pour 10K timers, suffisant pour nos besoins.

**Si on en avait besoin** : ~500 lignes, hierarchical timer wheel.
Intégrable dans un `AsyncActor` dédié aux timers.

### Écosystème réseau (Hyper, Axum, Tower)
Tokio a un écosystème HTTP complet. Pour un serveur WebSocket embarqué
(sync LUCIDS), on pourrait le faire avec `mio` + un parser WebSocket
léger, sans l'écosystème tokio.

## Étapes d'implémentation

### Phase 1 : Core (dans luciole/)

1. `LucioleWaker` — implémente `std::task::Wake`, redirige vers `WakeHandle`
2. `FutureSlot` — stockage + polling des futures
3. `AsyncActor` — actor standard qui drive des futures dans `poll_idle()`
4. `AsyncScope` — API pour soumettre des futures
5. `FutureHandle<T>` — attente coopérative du résultat
6. Tests unitaires : spawn + wait, spawn_detached, priorités

### Phase 2 : Bridges WASM

7. `JsPromiseFuture` — wrapper Future autour d'une JS Promise
   (SharedArrayBuffer + AtomicU32 pour le signaling)
8. `opfs_write` / `opfs_read` — fonctions async utilisant sync access handles
9. Bridge `fetch()` → Future Rust

### Phase 3 : Intégration lucivy

10. `OpfsActor` utilisant `AsyncScope` pour les writes
11. `SyncActor` pour LUCIDS delta (fetch + apply + persist)
12. Remplacement du thread dédié `lucivy_commit_async` par un future
