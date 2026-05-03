# Design : Suspend Everywhere — Éliminer le cooperative wait en multi-thread

Date : 3 mai 2026

## Contexte

Le `cooperative wait` est la cause racine des deadlocks en emscripten.
Le pattern : un thread fait `run_one_step()` en boucle pour pomper le scheduler
en attendant une reply. Problème : le WorkItem poppé peut lui-même faire un
cooperative wait → nesting → le thread est capturé à chaque niveau → deadlock
quand les dépendances forment un cycle.

Le `ActorStatus::Suspend` résout ce problème pour les actors : l'actor rend le
thread immédiatement, un `ResumeHandle` le relance quand la dépendance est
résolue. Zéro nesting possible.

**Constat** : le cooperative wait existe parce que du code non-actor (threads
externes, tasks) a besoin d'attendre des résultats. La solution structurelle
est de faire passer toutes les attentes par des actors (Suspend) ou par
`wait_blocking` (threads externes).

## Principe

```
AVANT :
  Thread externe → cooperative wait (pompe run_one_step, nesting possible)
  Actor handler → cooperative wait (INTERDIT mais existait, nesting)
  Task → cooperative wait (nesting)

APRÈS :
  Thread externe → wait_blocking (dort sur condvar, zéro pompage)
  Actor handler → Suspend + ResumeHandle (rend le thread, zéro nesting)
  Task → JAMAIS de wait (si une task a besoin d'attendre, c'est un actor)
```

## Composant clé : JoinResume

Aujourd'hui `ResumeHandle` est 1:1 — un reply → un resume. Pour le pattern
scatter (envoyer N messages, reprendre quand tous répondent), il faut un
`JoinResume` N:1.

```rust
/// Resume handle that fires only when all N replies have arrived.
pub struct JoinResume {
    remaining: AtomicUsize,
    handle: ResumeHandle,
}

impl JoinResume {
    /// Create a JoinResume that expects `count` completions.
    pub fn new(count: usize, handle: ResumeHandle) -> Arc<Self>;

    /// Create a per-reply ResumeHandle. Each one decrements the counter.
    /// The last one (remaining == 0) fires the actual ResumeHandle.
    pub fn one_shot(self: &Arc<Self>) -> ResumeHandle;
}
```

Fichier : `luciole/src/reply.rs`

## Flux commit refactoré

### Avant (deadlock)

```
Commit thread (externe, pas scheduler)
  → ShardedHandle::commit()
    → drain_pipeline() [cooperative wait × readers + router]
    → Pool::scatter(CommitMsg) [cooperative wait × 4 shards]
      → run_one_step poppe ShardActor
      → ShardActor::handle(Commit) → submit_task → Suspend
      → run_one_step poppe la TASK
      → task = writer.commit() → prepare_commit()
        → flush_indexers → COOPERATIVE WAIT IMBRIQUÉ ← DEADLOCK
```

### Après (zéro cooperative wait)

```
Commit thread (externe)
  → envoie CommitMsg à un CommitCoordinator (ou directement scatter + wait_blocking)
  → wait_blocking (dort sur condvar)

Scheduler thread A poppe ShardActor[0]
  → ShardActor::handle(Commit)
    → IndexWriter::flush_async() : envoie FlushMsg aux indexers
    → JoinResume(N_indexers, ctx.resume_handle())
    → chaque ReplyReceiver.set_resume(join.one_shot())
    → return Suspend (rend le thread A immédiatement)

Scheduler thread B poppe IndexerActor[0]
  → handle(FlushMsg) → finalize_segment inline → reply
  → reply.send() → JoinResume décompte → pas encore 0

Scheduler thread C poppe IndexerActor[1]
  → handle(FlushMsg) → finalize_segment inline → reply
  → reply.send() → JoinResume décompte → 0 ! → ResumeHandle fire
  → ShardActor[0] pushed dans ready_queue

Scheduler thread A poppe ShardActor[0] (resumed)
  → poll_idle() → tous les flush OK → continue commit
  → segment_updater.schedule_commit() inline
  → reader.reload()
  → reply.send(Ok(())) au commit thread

Commit thread se réveille (condvar notifiée par reply.send)
```

**Zéro nesting. Zéro cooperative wait. Les scheduler threads ne sont jamais
capturés plus longtemps qu'un handler/finalize.**

## Étapes d'implémentation

### 1. JoinResume dans reply.rs (~30 lignes)

Nouveau type `JoinResume` avec `new(count, handle)` et `one_shot()`.
Tests unitaires.

### 2. IndexWriter::flush_non_blocking() dans index_writer.rs

Nouvelle méthode qui envoie FlushMsg à tous les workers et retourne
les `Vec<ReplyReceiver>` SANS attendre. L'appelant (ShardActor) fait
le wait via Suspend.

```rust
impl IndexWriter {
    /// Send FlushMsg to all indexer workers. Returns receivers.
    /// Caller is responsible for waiting on the results.
    pub fn flush_workers(&mut self) -> Result<Vec<ReplyReceiver<...>>, LucivyError> {
        let mut receivers = Vec::new();
        for i in 0..self.worker_pool.len() {
            let (env, rx) = IndexerFlushMsg.into_request();
            self.worker_pool.worker(i).send(env)?;
            receivers.push(rx);
        }
        Ok(receivers)
    }
}
```

`prepare_commit()` utilise `flush_workers()` + wait (blocking ou cooperative
selon le contexte). Mais le chemin critique (ShardActor commit) utilisera
`flush_workers()` + JoinResume + Suspend.

### 3. ShardActor refactoré — commit sans task

Plus besoin de `submit_task` pour le commit. Le ShardActor fait tout
directement dans son handler, en non-bloquant :

```rust
ShardMsg::Commit { reply } => {
    // Phase 1 : flush indexers (non-bloquant)
    let mut guard = self.handle.writer.lock().unwrap();
    let writer = guard.as_mut().unwrap();
    let flush_rxs = writer.flush_workers().unwrap();

    // Phase 2 : JoinResume — reprendre quand tous les flush sont finis
    let join = JoinResume::new(flush_rxs.len(), ctx.resume_handle());
    for rx in &flush_rxs {
        rx.set_resume(join.one_shot());
    }

    self.pending_commit = Some(PendingCommit { flush_rxs, reply, guard });
    return ActorStatus::Suspend;
}
```

Au resume (poll_idle), le ShardActor :
1. Collecte les résultats des flush
2. Fait `prepared_commit.commit()` (synchrone, rapide — juste écrire le meta)
3. `reader.reload()`
4. Envoie la reply

### 4. Pool::scatter — mode wait_blocking pour multi-thread

```rust
pub fn scatter<R>(&self, make_msg, label) -> Vec<R> {
    let scheduler = global_scheduler();
    let receivers = send_to_all(make_msg);

    if scheduler.is_single_threaded() {
        // Single-thread : cooperative wait (seul moyen de progresser)
        receivers.map(|rx| rx.wait_cooperative_named(label, || scheduler.run_one_step())).collect()
    } else {
        // Multi-thread : wait_blocking (les scheduler threads font le boulot)
        receivers.map(|rx| rx.wait_blocking()).collect()
    }
}
```

Idem pour `Pool::drain`, `Pool::shutdown`, `ActorRef::request`.

### 5. Nettoyage

- Supprimer les diag eprintln temporaires
- Remettre DEFAULT_MIN_NUM_SEGMENTS à 8
- Supprimer le `submit_task` dans ShardActor (plus nécessaire)
- Le cooperative wait reste dans le code pour le mode single-thread
  mais n'est plus jamais atteint en multi-thread

## Fichiers impactés

| Fichier | Changement |
|---------|-----------|
| `luciole/src/reply.rs` | + JoinResume |
| `luciole/src/pool.rs` | scatter/drain/shutdown → wait_blocking en multi-thread |
| `luciole/src/mailbox.rs` | request → wait_blocking en multi-thread |
| `src/indexer/index_writer.rs` | + flush_workers() non-bloquant |
| `lucivy_core/src/sharded_handle.rs` | ShardActor commit via flush_workers + JoinResume + Suspend |
| `src/indexer/log_merge_policy.rs` | Remettre MIN_NUM_SEGMENTS à 8 |

## Risques et points d'attention

1. **MutexGuard across Suspend** : le ShardActor lock `handle.writer` dans le
   handler et doit le garder jusqu'au resume (poll_idle). MutexGuard n'est pas
   Send → il faut soit restructurer le lock, soit stocker le writer autrement.
   Solution probable : prendre le writer avec `take()` pendant le commit,
   le remettre au resume.

2. **Single-thread regression** : s'assurer que le mode single-thread
   (tests, WASM sans pthreads) fonctionne toujours avec cooperative wait.

3. **prepare_commit synchrone** : l'étape après flush (créer le
   PreparedCommit, écrire meta) est synchrone et rapide. Pas besoin de Suspend.
   Le ShardActor fait ça dans poll_idle après le resume.

4. **Segment updater** : `schedule_commit()` est déjà inline dans le
   segment_updater_actor. Pas de changement nécessaire.

5. **drain_pipeline** : le drain des readers et du router peut aussi
   utiliser wait_blocking. Les readers et router n'ont pas de dépendances
   complexes — drain = "traite ton mailbox puis reply". Simple.
