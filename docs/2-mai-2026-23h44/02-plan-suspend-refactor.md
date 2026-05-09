# Plan — ActorStatus::Suspend (2 mai 2026)

## Objectif

Éliminer tout cooperative waiting dans les handlers d'actors. Un handler
qui attend un autre actor retourne `Suspend(ResumeHandle)` au lieu de
boucler sur `run_one_step()`. Le thread est libéré immédiatement.

## Étapes

### 1. ResumeHandle dans reply.rs

Nouveau type `ResumeHandle` qui encapsule un callback de réveil.
Quand `Reply::send()` est appelé, si un ResumeHandle est enregistré,
il fire automatiquement.

```rust
pub struct ResumeHandle {
    inner: Arc<ResumeInner>,
}

struct ResumeInner {
    callback: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl ResumeHandle {
    pub fn fire(&self) { ... }
}
```

Sur `ReplyReceiver` :
- `set_resume(&self, handle: ResumeHandle)` — enregistre le callback
- `take_value(&self) -> Option<T>` — récupère la valeur sans bloquer

Sur `Reply::send()` et `Drop for Reply` : fire le ResumeHandle si présent.

**Tests** : send avec resume, drop avec resume, take_value.

### 2. ActorStatus::Suspend dans lib.rs

Ajouter le variant :
```rust
pub enum ActorStatus {
    Continue,
    Stop,
    Yield,
    Suspend(ResumeHandle),
}
```

### 3. Scheduler : gestion du Suspend

Dans `handle_batch` :
- Quand `try_handle_one()` retourne `Some(Suspend(handle))` :
  - Arrêter le batch (comme Stop/Yield)
  - Remettre l'actor dans le HashMap **sans** le pusher dans la
    ready_queue
  - Le ResumeHandle contient un callback qui push l'actor dans la
    ready_queue + notify_one quand il fire

Dans `run_one_step_actor` : même logique.

Nouveau dans `ActorSlot` : rien de spécial — l'actor est juste "parké"
(présent dans le HashMap, pas dans la ready_queue, is_idle=false).
Le ResumeHandle le réveille.

Construction du ResumeHandle dans le scheduler :
```rust
impl Scheduler {
    pub fn make_resume_handle(&self, actor_id: ActorId) -> ResumeHandle {
        let shared = Arc::clone(&self.shared);
        ResumeHandle::new(move || {
            // Push actor to ready_queue
            let priority = { ... get from actors map ... };
            let mut queue = shared.ready_queue.lock().unwrap();
            queue.push(WorkItem::Actor { priority, actor_id });
            shared.work_available.notify_one();
        })
    }
}
```

**Décision** : pattern Context (comme Actix `Context<Self>`).

Passer un `&ActorContext` à chaque appel de `handle()`. C'est le
pattern le plus générique et celui qui emmerde le moins à long terme :
- Aujourd'hui : `ctx.resume_handle()` pour Suspend
- Demain : `ctx.actor_id()`, `ctx.spawn_child()`, `ctx.self_ref()`,
  `ctx.deadline()`, etc.
- **Jamais besoin de retoucher la signature du trait** — on ajoute
  des méthodes à `ActorContext`

Les alternatives (thread-local, stored ref) sont des dettes : chaque
nouvelle capacité demande un nouveau mécanisme ad hoc.

```rust
trait Actor: Send {
    type Msg: Send;
    fn handle(&mut self, msg: Self::Msg, ctx: &ActorContext) -> ActorStatus;
    // ...
}

pub struct ActorContext {
    actor_id: ActorId,
    shared: Arc<SharedState>,
}

impl ActorContext {
    /// Crée un ResumeHandle qui replanifie cet actor dans le scheduler.
    pub fn resume_handle(&self) -> ResumeHandle { ... }
    
    /// L'identifiant de cet actor.
    pub fn actor_id(&self) -> ActorId { self.actor_id }
}
```

**Coût** : une migration mécanique — ajouter `_ctx: &ActorContext`
à chaque impl de `handle()`. Le compilateur garantit qu'on oublie rien.
Après c'est posé pour toujours.

### 4. Migration du trait Actor

Ajouter `ctx: &ActorContext` au trait Actor::handle. Le compilateur
force la mise à jour de tous les impls. Changement mécanique :

**Luciole :**
- GenericActor — passe le ctx à travers les TypedHandlers
- Tous les actors dans les tests (`_ctx: &ActorContext`)

**Lucivy (via GenericActor/TypedHandler) :**
- IndexerActor, FinalizerActor — TypedHandler reçoit ctx
- ShardActor, RouterActor, ReaderActor — `_ctx`
- SegmentUpdater, Merger actors — `_ctx`

Note : GenericActor et TypedHandler absorbent le ctx — les closures
des handlers métier reçoivent `ctx` en paramètre additionnel mais la
plupart l'ignorent (`_ctx`). Seul l'IndexerActor l'utilise
pour `ctx.resume_handle()`.

### 5. IndexerActor : utilise Suspend

**handle_docs** : quand mem_budget atteint :
```rust
// Avant : self.finalize_current_segment_background() → cooperative wait
// Après :
self.send_finalize_to_background();  // envoie FinalizeMsg, stocke rx
self.pending_rx.set_resume(ctx.resume_handle());
return ActorStatus::Suspend(ctx.resume_handle());
```

Au retour (prochain handle_docs appelé après resume) :
```rust
// En début de handle_docs : drain le pending finalize si résolu
if let Some(rx) = &self.pending_finalize {
    if let Some(val) = rx.take_value() {
        self.pending_finalize = None;
        // traiter le résultat (erreur?)
    }
}
```

**handle_flush** : si pending_finalize existe :
```rust
self.finalize_current_segment_blocking()?;
if self.pending_finalize.is_some() {
    self.deferred_flush_reply = Some(reply);
    self.pending_rx.set_resume(ctx.resume_handle());
    return ActorStatus::Suspend(ctx.resume_handle());
}
reply.send(FlushReply);
```

Au resume, `poll_idle()` envoie la deferred reply :
```rust
fn poll_idle(&mut self) -> Poll<()> {
    if let Some(reply) = self.deferred_flush_reply.take() {
        if let Some(rx) = self.pending_finalize.take() {
            let _ = rx.take_value(); // consume
        }
        reply.send(FlushReply);
    }
    Poll::Pending
}
```

### 6. Supprimer wait_cooperative des chemins indexer

- `wait_pending_finalize()` n'est plus appelé depuis les handlers
- `finalize_current_segment_background()` n'appelle plus
  `wait_pending_finalize()` (fire and forget + Suspend)
- `wait_cooperative` reste disponible pour les callers externes
  (flush_indexer dans prepare_commit, Pool::scatter, etc.)

### 7. Tests

- Test luciole : actor qui Suspend, resume, continue
- Test luciole : actor qui Suspend pendant un batch, batch reprend
- Test indexer : finalize via Suspend (pas de cooperative wait)
- Test emscripten : rebuild + playground (4 shards, rag3db clone)

### 8. Nettoyage

- Supprimer tous les `eprintln!("[diag]` temporaires
- Supprimer le helper thread? Non — le garder comme safety net pour
  d'éventuels futurs cooperative waits (Pool::scatter, etc.)

## Ordre d'exécution

```
1. reply.rs     — ResumeHandle type
2. lib.rs       — Suspend variant
3. scheduler.rs — ActorContext, handle Suspend, make_resume_handle
4. Actor trait  — ajouter ctx param (migration mécanique)
5. indexer_actor — state machine avec Suspend
6. Tests
7. Build emscripten + test playground
8. Nettoyage diag logs
```

## Risques

- **Migration du trait Actor** : touche beaucoup de fichiers mais
  c'est mécanique (_ctx ajouté partout). Risque : oublier un impl.
  Le compilateur nous dit.
- **State machine indexer** : la logique handle_docs + handle_flush
  + poll_idle doit être cohérente. Bien tester le cycle
  Suspend → resume → process next doc.
- **ResumeHandle et Drop** : si un actor est droppé pendant un
  Suspend, le ResumeHandle ne doit pas paniquer. Gérer le cas
  "actor removed while suspended".
