# Rapport Phase 2 : IndexerActor — TERMINÉ

## Résultat

Phase 2 du doc 07 implémentée. Le `worker_loop` est remplacé par `IndexerActor`
implémentant le trait `Actor`. L'`IndexWriter` utilise le `Scheduler` pour gérer
les threads d'indexation. 1085 tests passent, 0 régression.

## Fichiers créés

### `src/indexer/indexer_actor.rs` (~170 lignes)

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
    pending_error: Option<crate::LucivyError>,
}

impl<D: Document> Actor for IndexerActor<D> {
    type Msg = IndexerMsg<D>;
    fn name(&self) -> &'static str { "indexer" }
    fn handle(&mut self, msg: IndexerMsg<D>) -> ActorStatus { ... }
    fn priority(&self) -> Priority {
        if self.current.is_some() { Priority::High } else { Priority::Low }
    }
}
```

Méthodes clés :
- `handle_docs` : crée segment si besoin, `skip_to` une seule fois, add docs, check mem budget
- `handle_flush` : renvoie `pending_error` si présente, sinon finalize + reply
- `handle_shutdown` : finalize + defuse bomb + Stop
- `set_error` : stocke l'erreur + drop bomb (tue IndexWriterStatus)

## Fichiers modifiés

### `src/indexer/index_writer.rs`

**Struct `IndexWriter`** — champs remplacés :
- Supprimé : `workers_join_handle`, `operation_sender`, `worker_flush_senders`, `worker_id`
- Ajouté : `worker_refs: Vec<ActorRef<IndexerMsg<D>>>`, `next_worker: Arc<AtomicUsize>`,
  `scheduler: Scheduler`, `scheduler_handle: Option<SchedulerHandle>`

**Méthodes réécrites :**

| Méthode | Avant | Après |
|---------|-------|-------|
| `new()` | `crossbeam_channel::bounded` + `start_workers()` | `Scheduler::new` + `spawn` par worker + `scheduler.start()` |
| `send_add_documents_batch()` | `operation_sender.send(WorkerMessage::Docs)` | Round-robin `worker_refs[idx].send(IndexerMsg::Docs)` |
| `prepare_commit()` | `oneshot::channel` + `flush_sender` + `harvest_worker_error` | `reply()` + `worker.send(IndexerMsg::Flush)` + `rx.wait_blocking()` |
| `rollback()` | `flush_sender` + `oneshot` sync | `reply()` + `IndexerMsg::Flush` + `wait_blocking()` |
| `wait_merging_threads()` | `operation_sender.send(Shutdown)` + `join_handle.join()` | `worker.send(IndexerMsg::Shutdown)` + drop `SchedulerHandle` |
| `Drop` | `operation_sender.send(Shutdown)` + `join_handle.join()` | `worker.send(IndexerMsg::Shutdown)` + drop `SchedulerHandle` |

**Méthodes supprimées :**
- `worker_loop` (remplacé par IndexerActor)
- `add_indexing_worker()` (remplacé par spawn dans new())
- `start_workers()` (idem)
- `operation_receiver()` (plus nécessaire)
- `harvest_worker_error()` (remplacé par `pending_error` dans l'acteur)

**Visibilité changée :**
- `finalize_segment` : `fn` → `pub(super) fn` (pour import par indexer_actor.rs)

### `src/indexer/index_writer_status.rs`

Simplifié : plus de `WorkerReceiver<D>`. Juste `is_alive: AtomicBool` + bomb pattern.
Constructeur changé de `From<WorkerReceiver<D>>` à `IndexWriterStatus::new()`.

### `src/indexer/mod.rs`

- Ajouté : `pub(crate) mod indexer_actor;`
- `AddBatch<D>` : changé de `type` privé à `pub(crate) type` (utilisé par indexer_actor)
- Supprimé : `WorkerMessage`, `WorkerSender`, `WorkerReceiver`, `FlushSender`, `FlushReceiver`,
  import `crossbeam_channel as channel`, `use crate::schema::document::Document`

## Bug résolu : propagation d'erreur (tokenizer non enregistré)

**Symptôme** : `test_show_error_when_tokenizer_not_registered` échouait —
`commit()` renvoyait `Ok(())` au lieu de l'erreur du tokenizer.

**Cause** : `handle_docs` avalait silencieusement les erreurs (`Err(_e) => return Continue`).
Le segment restait `Some(...)` mais sans docs. `finalize_segment` voyait `max_doc == 0`
et renvoyait `Ok(())`.

**Fix** : Ajout de `pending_error: Option<LucivyError>` dans `IndexerActor`.
Quand `add_document` ou `SegmentWriter::for_segment` échoue :
1. L'erreur est stockée dans `pending_error`
2. La bomb est droppée → `IndexWriterStatus::is_alive()` retourne `false`
3. Les futurs `send_add_documents_batch` échouent immédiatement
4. Le prochain `Flush` renvoie l'erreur stockée

## Victoire FIFO

Le handle_flush passe de ~30 lignes (drain `try_recv` + `crossbeam::select!`) à 5 lignes :

```rust
fn handle_flush(&mut self, reply: Reply<crate::Result<()>>) -> ActorStatus {
    let result = if let Some(err) = self.pending_error.take() {
        Err(err)
    } else {
        self.finalize_current_segment()
    };
    reply.send(result);
    ActorStatus::Continue
}
```

La FIFO de la mailbox garantit que tous les `Docs` envoyés avant un `Flush` sont
traités avant le `Flush` — plus besoin du hack `try_recv` drain.

## Distribution round-robin vs MPMC

Avant : un channel MPMC partagé, les workers se disputent les docs.
Après : round-robin `next_worker.fetch_add(1) % len`, chaque worker a sa propre mailbox.

Avantages :
- Pas de contention sur le channel partagé
- Meilleure localité cache (chaque worker traite ses docs séquentiellement)
- La backpressure est par-worker (mailbox bounded)

## Validation

- `cargo check` : ✓ (warnings attendus sur variants/champs non utilisés du framework actor)
- `cargo test --lib` : 1085 passés, 0 échoué, 7 ignorés
- `cargo test` (doctests + compile-fail) : 50 + 2 passés
- **Total : 1137 tests, 0 régression**

## Prochaine étape

Phase 3 : SegmentUpdaterActor — porter le `SegmentUpdater` vers un acteur.
