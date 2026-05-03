# Progression session 3 mai 2026 — Suspend Everywhere

## Ce qui a été fait

### 1. JoinResume (reply.rs)
Resume N:1 — fire quand N sub-handles ont tous fire. Utilisé par ShardActor
pour attendre N indexer flushes puis Suspend.

### 2. scheduler.wait() — mode hybride
- Single-thread → cooperative wait (pompe run_one_step)
- Multi-thread + scheduler thread → cooperative wait  
- Multi-thread + external thread → wait_blocking (condvar)

Migré PARTOUT : Pool, ActorRef, TypedActorRef, FutureHandle, execute_dag,
IndexWriter, SegmentUpdater, IndexerActor.

### 3. ShardActor refactoré — flush_workers + JoinResume + Suspend
Plus de submit_task pour le commit. Le shard :
1. flush_workers() envoie FlushMsg aux indexers (non-bloquant)
2. JoinResume(N, ctx.resume_handle())
3. Suspend
4. poll_idle : collecte résultats, finalize_flush_and_prepare, commit, reply

### 4. IndexWriter::flush_workers() + finalize_flush_and_prepare()
Deux nouvelles méthodes pour séparer envoi/attente/finalisation.

### 5. Merges déférées dans segment_updater
handle_commit ne fait plus de merge inline. Le commit (save_metas) est
rapide. Les merges sont déférées (opt-in via drain_merges).

### 6. Indexer Yield après finalize
handle_docs retourne Yield si finalize_current_segment_blocking a eu lieu,
pour libérer le scheduler thread.

### 7. drain_pipeline drains shards
Ajout de shard_pool.drain("drain_shards") après drain readers + router.
Sinon les mailboxes des shards ont encore des centaines d'InsertMsg au
moment du CommitMsg.

### 8. Diagnostics améliorés
- wait_blocking_with_diag : utilise condvar.wait_timeout au lieu de
  thread::spawn (safe WASM, pas de pthread leak)
- Dump threads + ready_queue + non-idle actors dans les warnings
- Logs dans scatter (mailbox depth, ready_queue), ShardActor (phase 1/2),
  indexer (handle_docs, finalize, handle_flush), finalize_segment
- Nettoyé TOUS les diag eprintln de la session précédente

## Bugs trouvés et corrigés

| Bug | Cause | Fix |
|-----|-------|-----|
| Deadlock commit (initial) | cooperative wait nesting | scheduler.wait() hybride |
| 4 scheduler threads capturés | handle_batch 1024 + finalize inline | Yield après finalize |
| drain_router bloqué | indexers en finalize, 4 threads capturés | Yield |
| Merges monopolisent 4 threads | submit_task × 4 shards | merges déférées |
| thread::spawn leak pthreads | wait_blocking_diag spawnait diag thread | wait_timeout à la place |
| Shards ont 100+ InsertMsg au commit | drain_pipeline ne drainait pas shards | ajout shard drain |

## Problème en cours

L'indexer est TAKEN pendant 5+ minutes avec q:0. Il est dans un handler
(probablement handle_flush → wait_pending_finalize → cooperative wait)
et ne progresse pas.

3 scheduler threads sont IDLE, 1 est ACTOR(indexer). La ready_queue est vide.

Hypothèse : le cooperative wait dans l'indexer fait run_one_step en boucle,
la queue est vide, les 3 threads IDLE sont en pop_work(condvar.wait).
Le FinalizerActor devrait être réveillé par le send, mais le notify_one
ne réveille pas les threads IDLE → les threads IDLE ne poppent jamais
le FinalizerActor.

**Piste** : problème de condvar notify entre pthreads emscripten.
Ou : le FinalizerActor n'est jamais envoyé (bug dans le send path).

## A investiguer

1. Est-ce que le FinalizerActor reçoit le message ? (log dans le send)
2. Est-ce que work_available.notify_one() réveille un thread ?
3. Est-ce que le cooperative wait de l'indexer voit le Reply ?
4. Est-ce que c'est juste un finalize lent (SFX build sur 500 docs en WASM) ?

## Architecture finale visée

Tout code qui attend un résultat doit passer par :
- **Actors** : Suspend + ResumeHandle (pas de wait, thread libéré)
- **Threads externes** : wait_blocking (condvar, pas de pompage)
- **Cooperative wait** : SEULEMENT en single-thread ou pour les cas
  legacy scheduler-thread (segment_updater, etc.)

Le cooperative wait reste mais ne devrait plus jamais causer de nesting
en multi-thread (scheduler.wait choisit automatiquement).
