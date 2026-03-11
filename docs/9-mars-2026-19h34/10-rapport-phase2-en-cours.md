# Rapport Phase 2 : IndexerActor — EN COURS

## État : compilation cassée, refactor en cours

La Phase 2 a été démarrée mais n'est PAS terminée. Le code ne compile pas
actuellement. Ce document résume exactement où on en est pour pouvoir
reprendre après compression de contexte.

## Ce qui est fait

### 1. Phase 1 complète (doc 09)

Le module `src/actor/` est terminé et testé (19/19 tests, 0 régression) :
- `mod.rs` — trait Actor, ActorStatus, Priority
- `mailbox.rs` — Mailbox, ActorRef, WakeHandle (idle flag)
- `reply.rs` — Reply, ReplyReceiver (bounded(1))
- `events.rs` — EventBus broadcast, SchedulerEvent
- `scheduler.rs` — Scheduler, SharedState, priority queue, run_loop, run_one_step

### 2. Phase 2 fichiers créés/modifiés

#### `src/indexer/indexer_actor.rs` — NOUVEAU, COMPLET

```rust
pub(crate) enum IndexerMsg<D: Document> {
    Docs(AddBatch<D>),
    Flush(Reply<crate::Result<()>>),
    Shutdown,
}

pub(crate) struct IndexerActor<D: Document> {
    segment_updater: SegmentUpdater,
    index: Index,
    mem_budget: usize,
    delete_cursor: DeleteCursor,
    bomb: Option<IndexWriterBomb<D>>,
    current: Option<SegmentInProgress>,
}

impl<D: Document> Actor for IndexerActor<D> { ... }
```

Méthodes clés :
- `handle_docs` : crée segment si besoin, `skip_to` une seule fois, add docs, check mem budget
- `handle_flush` : **4 lignes** — finalize + reply (FIFO garanti, pas de drain)
- `handle_shutdown` : finalize + defuse bomb + Stop
- `priority()` : High si segment ouvert, Low sinon

#### `src/indexer/index_writer_status.rs` — RÉÉCRIT

Simplifié : plus de `WorkerReceiver<D>`. Juste `is_alive: AtomicBool` + bomb pattern.
Constructeur changé de `From<WorkerReceiver<D>>` à `IndexWriterStatus::new()`.

#### `src/indexer/index_writer.rs` — EN COURS DE MODIFICATION

**Ce qui a été changé :**

1. **Imports** : remplacés — plus de `WorkerMessage`, `WorkerSender`, `FlushSender`, etc.
   Ajouté : `IndexerActor`, `IndexerMsg`, `mailbox`, `reply`, `ActorRef`, `Scheduler`, `SchedulerHandle`

2. **Struct IndexWriter** : remplacée
   - Supprimé : `workers_join_handle`, `operation_sender`, `worker_flush_senders`, `worker_id`
   - Ajouté : `worker_refs: Vec<ActorRef<IndexerMsg<D>>>`, `next_worker: Arc<AtomicUsize>`,
     `scheduler: Scheduler`, `scheduler_handle: Option<SchedulerHandle>`

3. **`worker_loop`** : supprimé (remplacé par un commentaire)

**Ce qui N'A PAS ENCORE été changé (reste à faire) :**

4. **`IndexWriter::new()`** — doit être réécrit :
   - Créer `Scheduler::new(num_worker_threads)`
   - Créer `IndexWriterStatus::new()` (plus de receiver)
   - Pour chaque worker : `let (mbox, mut aref) = mailbox(PIPELINE_MAX_SIZE_IN_DOCS)`,
     créer `IndexerActor::new(...)`, `scheduler.spawn(actor, mbox, &mut aref, capacity)`
   - `scheduler.start()` → stocker le `SchedulerHandle`

5. **`add_indexing_worker()`** — à supprimer (remplacé par spawn dans new())

6. **`start_workers()`** — à supprimer

7. **`operation_receiver()`** — à supprimer

8. **`send_add_documents_batch()`** — à réécrire :
   ```rust
   fn send_add_documents_batch(&self, add_ops: AddBatch<D>) -> crate::Result<()> {
       if !self.index_writer_status.is_alive() {
           return Err(error_in_index_worker_thread("An index writer was killed."));
       }
       let idx = self.next_worker.fetch_add(1, Ordering::Relaxed) % self.worker_refs.len();
       self.worker_refs[idx].send(IndexerMsg::Docs(add_ops))
           .map_err(|_| error_in_index_worker_thread("An index writer was killed."))
   }
   ```

9. **`prepare_commit()`** — à réécrire :
   ```rust
   pub fn prepare_commit(&mut self) -> crate::Result<PreparedCommit<'_, D>> {
       let mut receivers = Vec::new();
       for worker in &self.worker_refs {
           let (reply_tx, reply_rx) = reply();
           worker.send(IndexerMsg::Flush(reply_tx))
               .map_err(|_| error_in_index_worker_thread("Worker died"))?;
           receivers.push(reply_rx);
       }
       for rx in receivers {
           rx.wait_blocking()?;  // En Phase 4: wait_cooperative pour single-thread
       }
       let commit_opstamp = self.stamper.stamp();
       Ok(PreparedCommit::new(self, commit_opstamp))
   }
   ```

10. **`harvest_worker_error()`** — à supprimer (le scheduler gère)

11. **`wait_merging_threads()`** — à adapter (envoyer Shutdown via actor_refs)

12. **`rollback()`** — à adapter :
    - Envoyer Flush à tous les workers, attendre
    - Kill segment_updater
    - Drop scheduler_handle
    - Recréer `*self = IndexWriter::new(...)`

13. **`Drop for IndexWriter`** — à réécrire :
    ```rust
    impl<D: Document> Drop for IndexWriter<D> {
        fn drop(&mut self) {
            self.segment_updater.kill();
            for worker in &self.worker_refs {
                let _ = worker.send(IndexerMsg::Shutdown);
            }
            // SchedulerHandle::drop() join les threads automatiquement
            self.scheduler_handle.take();
        }
    }
    ```

14. **`src/indexer/mod.rs`** — à mettre à jour :
    - Ajouter `pub(crate) mod indexer_actor;`
    - Garder `WorkerMessage`, `WorkerSender`, `WorkerReceiver`, `FlushSender`, `FlushReceiver`
      temporairement si d'autres fichiers les utilisent, ou les supprimer si
      `index_writer.rs` était le seul consommateur

## Points d'attention pour la reprise

1. **`finalize_segment`** est une fonction libre dans `index_writer.rs` qui doit rester
   accessible par `IndexerActor`. Elle est actuellement `fn` (private). L'import dans
   `indexer_actor.rs` fait `use crate::indexer::index_writer::finalize_segment` — il faut
   la rendre `pub(crate)` ou `pub(super)`.

2. **`MARGIN_IN_BYTES`** est aussi importé par `indexer_actor.rs` depuis `index_writer.rs`.
   Déjà `pub const`, OK.

3. **Le type `AddBatch<D>`** est défini dans `mod.rs` comme `type AddBatch<D> = SmallVec<[AddOperation<D>; 4]>`.
   L'IndexerActor l'utilise via `super::AddBatch`.

4. **Les tests dans `index_writer.rs`** (lignes 938+) ne doivent PAS être modifiés.
   Ils testent l'API publique (`add_document`, `commit`, `rollback`, etc.) qui ne change pas.

5. **`IndexWriterBomb<D>`** est maintenant dans le `IndexWriterStatus` simplifié.
   L'import dans `indexer_actor.rs` est `use crate::indexer::index_writer_status::IndexWriterBomb`.

6. **Scheduler lifetime** : le `Scheduler` est owned par `IndexWriter`. Les `ActorRef`
   contiennent des `Arc<WakeHandle>` qui référencent le `SharedState` du scheduler.
   Tout est géré par Arc, pas de lifetime issue.

## Validation attendue

Après avoir terminé les points 4-14 ci-dessus :
- `cargo check` doit compiler
- `cargo test` doit passer les 1137 tests existants sans modification
- Les tests de `index_writer_status.rs` passent déjà (réécrits, 2 tests)
