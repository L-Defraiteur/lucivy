# Progression — Framework Suspend (3 mai 2026, 00h30)

## Ce qui est fait

### reply.rs — ResumeHandle

Nouveau type `ResumeHandle` : callback `FnOnce() + Send + 'static` qui
re-schedule un actor suspendu. Clonable (Arc interne), idempotent (fire
une seule fois).

`Reply::send()` et `Drop for Reply` firent le ResumeHandle si enregistré.
Garantit que l'actor suspendu est toujours réveillé, même si l'actor
dépendant meurt sans répondre.

`ReplyReceiver` : nouvelles méthodes `set_resume()`, `take_value()`,
`is_ready()`.

### lib.rs — ActorStatus::Suspend

```rust
pub enum ActorStatus {
    Continue,
    Yield,
    Stop,
    Suspend,  // NOUVEAU
}
```

### lib.rs — Actor::handle avec ActorContext

```rust
fn handle(&mut self, msg: Self::Msg, ctx: &ActorContext) -> ActorStatus;
```

Pattern Context (comme Actix). Extensible : on ajoute des méthodes à
ActorContext sans jamais retoucher le trait.

### scheduler.rs — ActorContext

```rust
pub struct ActorContext {
    actor_id: ActorId,
    shared: Arc<SharedState>,
}

impl ActorContext {
    pub fn actor_id(&self) -> ActorId { ... }
    pub fn resume_handle(&self) -> ResumeHandle { ... }
}
```

`resume_handle()` crée un callback qui push l'actor dans la ready_queue
avec `notify_one()`. Capturé dans un `Arc<SharedState>` pour être
`'static + Send`.

### scheduler.rs — Gestion de Suspend

`handle_batch` : quand `try_handle_one` retourne `Suspend` →
`BatchResult::Suspended`. L'actor est remis dans le HashMap SANS être
pushé dans la ready_queue. Le ResumeHandle (enregistré par le handler
via `rx.set_resume(ctx.resume_handle())`) le replanifiera.

`run_one_step_actor` : même logique.

### Passage &SharedState → &Arc<SharedState>

Toutes les fonctions internes du scheduler (`run_loop`, `handle_batch`,
`pop_work`, `helper_loop`, `run_one_step_impl`, `run_one_step_actor`,
`emit_priority_change`) prennent maintenant `&Arc<SharedState>` pour
que `ActorContext` puisse capturer un `Arc::clone`.

### Migration du trait Actor

Tous les `impl Actor` et `TypedHandler` closures migrés :

**Luciole (framework) :**
- `generic_actor.rs` — dispatch passe ctx
- `async_executor.rs` — `_ctx`
- Tests : Counter, PrioActor, IdleWorker, SelfSender, PingPong, LogActor,
  Worker, CountWorker (scope, stream_dag, pool)
- `handler.rs` tests : dummy ctx via `Scheduler::test_context()`

**Lucivy (métier) :**
- `segment_updater_actor.rs` — 5 closures (`_ctx`)
- `indexer_actor.rs` — 4 closures (`_ctx`)
- `sharded_handle.rs` — 9 closures (`_ctx`)

### Tests

- **138 tests luciole** : OK
- **1208+ tests ld-lucivy** : OK (1 flaky proptest pré-existant)

## Ce qui reste

1. **Guard cooperative wait** — thread-local `IN_ACTOR_HANDLER`,
   panic en debug si `wait_cooperative` appelé depuis un handler
2. **State machine IndexerActor** — utiliser Suspend au lieu de
   `wait_pending_finalize()`
3. **Retirer le helper thread** — plus nécessaire avec Suspend
4. **Nettoyage diag logs** — supprimer tous les `eprintln!("[diag]`
5. **Tests Suspend** — test basique suspend/resume dans luciole
6. **Build emscripten + test playground**
