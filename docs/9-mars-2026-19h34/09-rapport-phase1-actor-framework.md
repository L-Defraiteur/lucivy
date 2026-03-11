# Rapport Phase 1 : Actor Framework — Fondations

## Résultat

Phase 1 du doc 07 implémentée. Le module `src/actor/` fournit un framework actor
minimal avec scheduler, mailbox FIFO, priority queue, reply oneshot, et observabilité
par events broadcast. 19 tests unitaires passent, 0 régression sur la suite complète
(1137 tests).

## Fichiers créés

```
src/actor/
  mod.rs        (65 lignes)   — trait Actor, ActorStatus, Priority, re-exports
  mailbox.rs    (80 lignes)   — Mailbox, ActorRef, WakeHandle, fn mailbox()
  reply.rs      (95 lignes)   — Reply, ReplyReceiver, fn reply() + 5 tests
  events.rs     (180 lignes)  — EventBus broadcast, SchedulerEvent, EventReceiver + 4 tests
  scheduler.rs  (530 lignes)  — Scheduler, SharedState, AnyActor, run_loop + 10 tests
```

**Total** : ~950 lignes (dont ~300 de tests).

## Fichier modifié

- `src/lib.rs` : ajout `mod actor;`

## Décisions de design prises pendant l'implémentation

### 1. Take pattern pour éviter les deadlocks réentrants

Le scheduler `take()` l'acteur hors du `HashMap<ActorId, ActorSlot>` pendant qu'il
appelle `handle()`. Le lock `actors` est libéré pendant le traitement. Un autre thread
(ou `run_one_step` en mode coopératif) peut accéder à d'autres acteurs sans deadlock.

C'est l'option 2 du doc 08 (point 3) : "Le scheduler prend l'acteur OUT du HashMap,
appelle handle, puis le remet."

Si un doublon de l'acteur est dans la ready queue (envoi pendant le traitement),
le thread qui le pop trouve `slot.actor = None` et passe au suivant.

### 2. WakeHandle avec idle flag

Le `WakeHandle` est un `Arc` partagé entre l'`ActorRef` et le `ActorSlot` du scheduler :

```
ActorRef::send()                    Scheduler (run_loop)
    │                                    │
    │  msg → channel                     │
    │  is_idle.swap(false)               │
    │  si était true → wake()            │  BatchResult::Idle →
    │     push ready_queue               │    is_idle.store(true)
    │     notify_one()                   │
```

- Au spawn : `is_idle = false` (l'acteur est déjà dans la ready queue).
- Le scheduler remet `is_idle = true` quand l'acteur passe idle après un batch.
- L'ActorRef fait `swap(false)` : si le flag était `true`, il wake le scheduler.
- Résultat : un seul wake par transition idle→active. Les sends suivants dans un
  burst ne spamment pas la condvar.

### 3. EventBus broadcast (pas MPMC partagé)

Premier essai avec `crossbeam_channel::Receiver::clone()` — MPMC, chaque message
va à UN seul reader. Les tests `test_multiple_subscribers` échouaient.

Fix : Vec de senders protégé par Mutex. Chaque `subscribe()` crée un channel dédié.
`emit()` clone l'event et l'envoie à chaque sender. Le Mutex n'est pris que sur emit
avec subscribers (rare en production — surtout pour les tests et le debug).

### 4. Batch vs single-step

- `run_loop` (multi-thread) : traite jusqu'à `BATCH_SIZE = 32` messages par acteur
  avant de yield au scheduler. Réduit le coût du lock actors par message.
- `run_one_step` (mode coopératif) : traite UN SEUL message. Rend la main vite
  pour que `Reply::wait_cooperative` puisse progresser sur d'autres acteurs.

### 5. Reply via crossbeam bounded(1)

Le doc 06 utilisait `oneshot` (futures-channel). On utilise `crossbeam_channel::bounded(1)`
pour rester sur une seule dépendance channel. Le overhead est négligeable (un channel
avec une seule valeur). Ça simplifie le futur remplacement de `FutureResult` (Phase 3).

## Tests couverts

| Test | Ce qu'il vérifie |
|------|-----------------|
| `test_actor_counter` | Spawn, send, reply, wait_blocking |
| `test_actor_stop` | ActorStatus::Stop retire l'acteur |
| `test_multi_thread` | 4 threads, 1000 messages, compteur correct |
| `test_single_thread_cooperative_reply` | run_one_step sans start(), wait_cooperative |
| `test_multiple_actors` | 2 acteurs sur 2 threads, indépendants |
| `test_priority_ordering` | High passe avant Low dans run_one_step |
| `test_events_received` | ActorSpawned + MessageHandled émis |
| `test_zero_cost_no_subscriber` | 10k messages sans subscriber, pas d'OOM |
| `test_scheduler_drop_shutdown` | Drop SchedulerHandle join proprement |
| `test_poll_idle_actor` | poll_idle appelé quand mailbox vide |
| `test_reply_*` (5 tests) | send/recv, try_recv, cooperative, panic on drop |
| `test_events_*` (4 tests) | broadcast, unsubscribe, multiple subscribers |

## Bugs rencontrés et résolus

### Bug #1 : EventBus MPMC au lieu de broadcast

**Symptôme** : `test_multiple_subscribers` FAILED — le deuxième subscriber ne recevait
pas les events.

**Cause** : `crossbeam_channel::Receiver::clone()` crée un consumer MPMC. Chaque message
va à un seul reader, pas à tous.

**Fix** : `Vec<Sender>` avec broadcast explicite dans `emit()`.

### Bug #2 : Hang dans test_multiple_actors et test_zero_cost_no_subscriber

**Symptôme** : Tests multi-thread bloqués indéfiniment. L'acteur traitait son premier
batch de messages, devenait idle, puis les messages suivants envoyés par `ActorRef::send()`
ne le réveillaient jamais.

**Cause** : Le flag `actor_was_idle` dans le notifier n'était jamais remis à `true` par
le scheduler après un batch idle. L'ActorRef faisait `swap(false)` → le flag était déjà
`false` → pas de wake → l'acteur restait idle indéfiniment.

**Fix** : Stocker le `WakeHandle` (contenant le flag) dans le `ActorSlot`. Le scheduler
fait `slot.wake_handle.is_idle.store(true)` dans la branche `BatchResult::Idle`.

## Prochaine étape

Phase 2 : IndexerActor — porter `worker_loop` vers `impl Actor for IndexerActor`.
Le handle_flush passe de 30 lignes (drain + try_recv) à 4 lignes (FIFO garanti).
