# Récap investigation deadlock — 3 mai 2026

## État du code

Branche `feature/unified-sharded-handle`. Dernier commit safe : `c037d41`.
Modifications non committées : tout le refactor Suspend/ActorContext.

## Ce qui a été ajouté (infrastructure solide, à garder)

### ActorContext — pattern Context pour luciole
- `Actor::handle(&mut self, msg, ctx: &ActorContext)` — 1209 tests passent
- `ctx.resume_handle()` → crée un callback qui replanifie l'actor
- `ctx.actor_id()` → identité de l'actor
- Extensible : ajouter des méthodes sans toucher le trait
- Fichiers : `luciole/src/lib.rs`, `luciole/src/scheduler.rs`
- Migration mécanique faite : tous les `impl Actor`, tous les `TypedHandler`

### ResumeHandle dans reply.rs
- `ReplyReceiver::set_resume(handle)` — enregistre un callback
- `Reply::send()` et `Drop for Reply` firent le handle
- `ReplyReceiver::take_value()`, `is_ready()` — accès non-bloquant

### ActorStatus::Suspend
- Scheduler gère Suspend dans `handle_batch` et `run_one_step_actor`
- Actor remis dans le HashMap sans push dans la ready_queue
- Le ResumeHandle push l'actor quand la dépendance complète

### GenericActor::with_poll_idle_fn
- Callback pour drainer le state après un resume
- Utilisé par le ShardActor pour envoyer la reply de commit

### Guard cooperative wait
- `IN_ACTOR_HANDLER` thread-local — set autour de `try_handle_one`
- `IN_COOPERATIVE_WAIT` thread-local — depth counter
- Warning en debug si cooperative wait dans un handler
- `in_cooperative_wait()` utilisé par `execute_dag` pour forcer inline

### Thread registry + Mermaid dump
- `ThreadInfo` + `THREAD_REGISTRY` global
- `register_thread()` dans `run_loop`
- `dump_mermaid()` sur Scheduler — graph threads + actors + queue
- Intégré dans le warning de cooperative wait (3 premiers warnings)
- Export C : `lucivy_dump_mermaid()`, `lucivy_dump_state()`

### Diag run_one_step
- Compteur + log dans `run_one_step_impl` (popped=actor/task/empty, queue_remaining)

## Le vrai problème identifié

### Symptôme
Le commit shardé (4 shards, ~500 docs par shard) bloque indéfiniment
ou prend 190s+ sans finir.

### Cause racine : le commit thread monopolise le travail

Le flux du commit standard :

```
1. Emscripten commit thread appelle ShardedHandle::commit()
2. commit() → drain_pipeline() → Pool::scatter(ShardCommitMsg)
3. Pool::scatter envoie 4 ShardCommitMsg, fait cooperative wait
   Fichier: luciole/src/pool.rs:112 (scatter)
   
4. Le cooperative wait appelle run_one_step en boucle
   Fichier: luciole/src/reply.rs:155 (wait_cooperative_named)
   
5. run_one_step poppe les ShardActor du ready_queue
   Fichier: luciole/src/scheduler.rs:939 (run_one_step_impl)
   
6. ShardActor::handle(ShardCommitMsg) → submit_task(writer.commit) → Suspend
   Fichier: lucivy_core/src/sharded_handle.rs:795 (ShardActor typed)
   
7. Les 4 tasks (writer.commit) sont dans la ready_queue
   
8. Le MÊME commit thread poppe les tasks via run_one_step !
   (Les scheduler threads n'ont pas le temps — run_one_step est non-bloquant,
   le commit thread les prend avant que pop_work/condvar se réveille)
   
9. Chaque task fait writer.commit() → prepare_commit() → flush_indexer
   Fichier: src/indexer/index_writer.rs:532 (flush_indexer)
   
10. flush_indexer envoie FlushMsg à l'indexer → cooperative wait
    Le commit thread est maintenant en cooperative wait IMBRIQUÉ
    (scatter wait → task → flush_indexer wait)
    
11. run_one_step depuis flush_indexer → queue vide (les indexers sont TAKEN
    par les scheduler threads qui font handle_batch)
    
12. DEADLOCK : le commit thread attend les indexers, les scheduler threads
    traitent les indexers mais ne peuvent pas finir (le segment_updater
    commit qui suivrait est lui aussi bloqué dans la chaîne)
```

### Le problème fondamental

`run_one_step` est **glouton** : il prend tout ce qu'il trouve dans la
queue (actors, tasks, n'importe quoi). Quand le commit thread poppe les
4 tasks writer.commit, il se retrouve avec 4 cooperative waits imbriqués
sur UN seul thread. Les scheduler threads sont soit IDLE (rien à popper)
soit occupés avec les indexers dans handle_batch.

### Pourquoi handle_batch empire les choses

Les scheduler threads prennent les indexers via `pop_work → handle_batch(1024)`.
Chaque handle_batch traite jusqu'à 1024 messages. Avec 500 docs par shard :
- handle_batch traite les 500 docs + 1 FlushMsg
- Chaque doc peut déclencher finalize_current_segment_blocking (budget mémoire)
- Chaque finalize prend 0.2-2s en WASM
- Le thread est capturé pour 30-120s

Pendant ce temps, le commit thread (qui a pris les tasks) fait cooperative
wait mais ne peut rien popper (queue vide).

## Pistes de solution pour la prochaine session

### Piste A : run_one_step ne doit PAS prendre les tasks

Le commit thread ne devrait popper que les ACTORS via run_one_step (pour
traiter les ShardCommitMsg et déclencher les Suspend). Les tasks
(writer.commit) devraient être laissées aux scheduler threads via pop_work.

Implémentation : `run_one_step_impl` skip les `WorkItem::Task` et ne
poppe que `WorkItem::Actor`. Ou : filtrer par priorité.

Problème : certains cooperative waits DOIVENT traiter des tasks
(execute_dag soumet des tasks et attend). Il faut distinguer.

### Piste B : ne pas utiliser submit_task pour ShardCommitMsg

Le ShardActor fait submit_task → Suspend. Mais si le commit thread
prend la task, c'est pire que si le ShardActor faisait le commit inline
(au moins le scheduler thread serait celui qui fait le travail).

Option : ShardActor fait writer.commit() inline. Le cooperative wait de
Pool::scatter pompe les scheduler threads (via run_one_step). Les
scheduler threads NE prennent PAS les ShardActors (ils sont déjà pris
par le commit thread via run_one_step_actor). Ça revient au même pb.

### Piste C : Pool::scatter utilise wait_blocking (pas cooperative)

Pool::scatter bloque le commit thread sur un condvar au lieu de pomper
run_one_step. Les scheduler threads font tout le travail (pop_work →
ShardActor → commit → flush → etc). Le commit thread ne fait que dormir.

Problème : les 4 ShardActor handlers font Suspend (submit_task). Les
tasks sont dans la queue. Les scheduler threads les poppent via pop_work.
Chaque thread prend une task, fait writer.commit() → flush_indexer →
cooperative wait → run_one_step → prend l'indexer → handle_batch.

Ça devrait marcher : 4 scheduler threads × 1 task chacun × 1 indexer
chacun. Plus de monopolisation par le commit thread. Le commit thread
dort sur le condvar et se réveille quand les 4 replies arrivent.

C'est probablement la meilleure piste. Simple, pas de changement
d'architecture. Juste changer Pool::scatter pour utiliser wait_blocking
au lieu de wait_cooperative.

ATTENTION : wait_blocking bloque sur le condvar du Reply, pas sur le
scheduler condvar. Les scheduler threads doivent faire `Reply::send()`
pour réveiller le commit thread. Ça marche déjà — Reply::send() fait
`inner.ready.notify_one()`.

### Piste D : commit_direct amélioré (séquentiel mais avec Suspend)

Revenir à commit_direct (séquentiel, 1 shard à la fois) mais avec
les améliorations Suspend. Chaque shard committe séquentiellement,
le commit thread fait cooperative wait pour chacun. Un seul cooperative
wait actif à la fois → pas de monopolisation.

Plus lent (séquentiel vs parallèle) mais simple et sûr.

### Piste E : affinité run_one_step

run_one_step_impl prend un paramètre "type filter" :
- `RunOneStepFilter::ActorsOnly` — ne poppe que les actors
- `RunOneStepFilter::TasksOnly` — ne poppe que les tasks
- `RunOneStepFilter::Any` — comme maintenant

Le scatter cooperative wait utilise `ActorsOnly`. Il poppe les
ShardActors, déclenche les Suspend, mais laisse les tasks aux scheduler
threads. Les flush_indexer cooperative waits (dans les tasks) utilisent
`Any` pour pouvoir traiter les indexers.

Plus flexible mais plus complexe.

## Diagnostic à améliorer

### Mermaid plus fin
- Ajouter les EDGES de dépendance : quel thread attend quel actor
  (via le ReplyReceiver → acteur source du Reply)
- Montrer la chaîne de cooperative waits (nesting depth + labels)
- Couleurs : rouge pour BUSY >10s, orange pour WAIT >10s

### Diag schedule_commit / merge
- Log le nombre de merge candidates à chaque cycle dans handle_commit
- Log le temps de chaque merge DAG execution
- Log les segments avant/après merge (num_docs, sizes)

### Diag handle_batch
- Log le nombre de messages traités par batch (pas juste start/end)
- Log quand finalize_current_segment_blocking est appelé + durée
- Log quand handle_flush commence/finit

## Fichiers clés à connaître

| Fichier | Ce qu'il fait |
|---------|--------------|
| `luciole/src/scheduler.rs` | run_loop, handle_batch, run_one_step_impl, pop_work, ActorContext, ThreadInfo |
| `luciole/src/reply.rs` | wait_cooperative_named, ResumeHandle, enter/leave_cooperative_wait |
| `luciole/src/runtime.rs:248` | execute_dag inline check (is_scheduler_thread, in_actor_handler, in_cooperative_wait) |
| `luciole/src/pool.rs:112` | Pool::scatter — envoie à tous les workers et wait cooperative |
| `lucivy_core/src/sharded_handle.rs:795` | ShardActor::handle(Commit) — submit_task + Suspend |
| `lucivy_core/src/sharded_handle.rs:1723` | ShardedHandle::commit() — drain + scatter |
| `src/indexer/index_writer.rs:514` | prepare_commit → flush_indexer (send FlushMsg + cooperative wait) |
| `src/indexer/indexer_actor.rs` | handle_docs (inline finalize), handle_flush |
| `src/indexer/segment_updater_actor.rs` | handle_commit (commit DAG inline), handle_merge |
| `src/indexer/log_merge_policy.rs:10` | DEFAULT_MIN_NUM_SEGMENTS = 2 (temporaire, remettre à 8) |

## État des tests

- `cargo test --lib` : 1209 passent, 0 échec (avec guard en warning, pas abort)
- `cargo test -p luciole --lib` : 138 passent
- Emscripten build OK
- Playground : rag3db 4K docs passe quand les mailboxes sont vides au commit,
  bloque quand il y a 500+ docs en attente dans les indexers
