# Rapport Phase 3 : SegmentUpdaterActor — EN COURS

## État : compile, tests non validés

Le code compile (`cargo check` OK, 24 warnings attendus). Les tests n'ont pas
pu être lancés (processus bloqué/timeout — même symptôme que la session
précédente avec des processus cargo zombies).

## Ce qui est fait

### 1. `src/actor/mailbox.rs` — Clone impl corrigée

`ActorRef<M>` avait `#[derive(Clone)]` qui génère `impl<M: Clone> Clone`.
Problème : nos messages contiennent `Reply<T>` qui n'est pas Clone.
Fix : impl Clone manuelle sans contrainte sur M (crossbeam Sender est Clone
pour tout M).

```rust
// Avant : #[derive(Clone)]
// Après :
impl<M> Clone for ActorRef<M> {
    fn clone(&self) -> Self {
        ActorRef {
            sender: self.sender.clone(),
            notifier: self.notifier.clone(),
        }
    }
}
```

### 2. `src/indexer/segment_updater_actor.rs` — NOUVEAU (~280 lignes)

```rust
pub(crate) enum SegmentUpdaterMsg {
    AddSegment { entry: SegmentEntry, reply: Reply<Result<()>> },
    Commit { opstamp, payload, reply: Reply<Result<Opstamp>> },
    GarbageCollect(Reply<Result<GarbageCollectionResult>>),
    StartMerge { merge_operation, reply: Reply<Result<Option<SegmentMeta>>> },
    EndMerge { merge_operation, merge_result, reply_to_caller: Option<Reply<...>> },
    Kill,
}

pub(crate) struct SegmentUpdaterActor {
    shared: Arc<SegmentUpdaterShared>,
    merge_thread_pool: ThreadPool,
    self_ref: Arc<SelfRefSlot>,
}

impl Actor for SegmentUpdaterActor { ... }
```

**SelfRefSlot** : `Arc<Mutex<Option<ActorRef>>>` rempli après `scheduler.spawn()`
quand le WakeHandle est attaché. Nécessaire pour que les merge threads puissent
envoyer `EndMerge` à l'acteur et le réveiller.

**Handlers portés** :
- `handle_add_segment` : add segment + consider_merge_options
- `handle_commit` : purge_deletes + commit + save_metas + GC + consider_merge_options
- `handle_garbage_collect` : garbage_collect_files
- `handle_start_merge` / `start_merge_impl` : segment_manager.start_merge + spawn sur merge_thread_pool
- `handle_end_merge` / `do_end_merge` : advance_deletes + segment_manager.end_merge + save_metas + consider_merge_options + GC
- `consider_merge_options` : compute candidates + start_merge_impl(None) pour chaque

**Victoire end_merge non-bloquant** : l'ancien `end_merge` bloquait le merge thread
en attendant le rayon pool (`schedule_task(...).wait()`). Maintenant le merge thread
envoie `EndMerge` à l'acteur (non-bloquant) et sort. L'acteur fait le travail
séquentiellement dans sa mailbox FIFO.

### 3. `src/indexer/segment_updater.rs` — RÉÉCRIT

**Struct `SegmentUpdaterShared`** (remplace `InnerSegmentUpdater`) :
- `active_index_meta`, `index`, `segment_manager`, `merge_policy`, `killed`, `stamper`, `merge_operations`
- Méthodes utilitaires : `save_metas`, `load_meta`, `store_meta`, `purge_deletes`, `get_mergeable_segments`, `list_files`

**Struct `SegmentUpdater`** (facade) :
- `shared: Arc<SegmentUpdaterShared>` + `actor_ref: ActorRef<SegmentUpdaterMsg>`
- Deref vers SegmentUpdaterShared (compatibilité API)
- `create()` prend maintenant un `&Scheduler` en paramètre

**Méthodes réécrites** :

| Méthode | Avant | Après |
|---------|-------|-------|
| `schedule_add_segment()` | `schedule_task` → `FutureResult<()>` | send `AddSegment` + `reply_rx.wait_blocking()` → `Result<()>` |
| `schedule_commit()` | `schedule_task` → `FutureResult<Opstamp>` | send `Commit` + `wait_blocking()` → `Result<Opstamp>` |
| `schedule_garbage_collect()` | `schedule_task` → `FutureResult<GC>` | send `GarbageCollect` + `wait_blocking()` → `Result<GC>` |
| `start_merge()` | `segment_manager.start_merge` + spawn + `FutureResult` | send `StartMerge` + `wait_blocking()` → `Result<Option<SegmentMeta>>` |
| `kill()` | set atomic | set atomic + send `Kill` |

**Supprimé** :
- `InnerSegmentUpdater` (remplacé par `SegmentUpdaterShared`)
- `schedule_task()` (remplacé par messages acteur)
- Dépendance `FutureResult` dans segment_updater
- rayon pool single-thread (`pool: ThreadPool`)

**Conservé** :
- `merge_thread_pool` rayon (dans l'acteur, Phase 5 le remplacera)
- Fonctions libres : `save_metas`, `merge`, `garbage_collect_files`, `merge_indices`, `merge_filtered_segments`
- Tous les tests existants

### 4. `src/indexer/index_writer.rs` — MODIFIÉ

- `IndexWriter::new()` : crée le `Scheduler` AVANT `SegmentUpdater::create()` (qui en a besoin)
- `finalize_segment()` : `.schedule_add_segment(entry).wait()?` → `.schedule_add_segment(entry)?`
- `add_segment()` : idem, supprimé `.wait()`
- `garbage_collect_files()` : retourne `crate::Result<GC>` au lieu de `FutureResult<GC>`
- `merge()` : retourne `crate::Result<Option<SegmentMeta>>` au lieu de `FutureResult<...>`
- Import `FutureResult` supprimé

### 5. `src/indexer/prepared_commit.rs` — SIMPLIFIÉ

- `commit_future()` supprimé
- `commit()` appelle directement `segment_updater().schedule_commit(opstamp, payload)`
- Import `FutureResult` supprimé

### 6. Callers `.merge().wait()` — TOUS MIS À JOUR (~27 occurrences)

Fichiers modifiés (suppression mécanique de `.wait()`) :
- `src/lib.rs` (2)
- `src/aggregation/bucket/histogram/date_histogram.rs` (1)
- `src/aggregation/mod.rs` (2)
- `src/core/tests.rs` (1)
- `src/store/mod.rs` (2)
- `src/store/index/mod.rs` (1)
- `src/query/term_query/term_scorer.rs` (1)
- `src/indexer/index_writer.rs` tests (5)
- `src/indexer/merger.rs` (9)
- `src/indexer/segment_writer.rs` (1)
- `src/indexer/merge_index_test.rs` (1)
- `src/fastfield/mod.rs` (3)

### 7. `src/indexer/mod.rs`

Ajouté : `pub(crate) mod segment_updater_actor;`

## Ce qui reste à faire

1. **Tests** : `cargo test --lib` n'a pas terminé (timeout/blocage). Probablement
   un deadlock dans le nouveau code. Hypothèses :
   - Le `SelfRefSlot` n'est pas initialisé à temps (merge thread essaie d'envoyer
     avant que le slot soit rempli)
   - Deadlock entre le scheduler et le `wait_blocking()` du SegmentUpdater
     (l'appelant bloque le thread scheduler qui devrait traiter le message)
   - Le `kill()` envoie `Kill` à l'acteur mais le scheduler est déjà en shutdown

2. **Debug du blocage** : lancer un test isolé pour identifier le deadlock :
   ```
   cargo test test_delete_during_merge -- --nocapture
   ```

3. **FutureResult** : reste utilisé dans `watch_event_router.rs` et `lib.rs`
   (re-export). Pas touché — c'est hors scope Phase 3.

## Architecture résultante

```
IndexWriter
  ├── worker_refs: Vec<ActorRef<IndexerMsg>>     (Phase 2)
  ├── scheduler: Scheduler
  ├── scheduler_handle: SchedulerHandle
  └── segment_updater: SegmentUpdater
        ├── shared: Arc<SegmentUpdaterShared>     (état partagé)
        └── actor_ref: ActorRef<SegmentUpdaterMsg>
              └── SegmentUpdaterActor             (dans le Scheduler)
                    ├── shared (même Arc)
                    ├── merge_thread_pool: ThreadPool (rayon)
                    └── self_ref: Arc<SelfRefSlot>
```

Flux d'un commit :
1. `IndexWriter::commit()` → `prepare_commit()` → Flush workers via Reply
2. `PreparedCommit::commit()` → `segment_updater.schedule_commit(opstamp, payload)`
3. SegmentUpdater envoie `Commit` msg → `reply_rx.wait_blocking()`
4. SegmentUpdaterActor traite : purge_deletes + commit + save_metas + GC + merges
5. Reply envoyé → le wait_blocking() retourne

## Point d'attention critique

Le problème probable est un **deadlock scheduler** :
- `finalize_segment()` est appelé depuis un `IndexerActor` (sur un thread scheduler)
- Il appelle `segment_updater.schedule_add_segment()` qui fait `reply_rx.wait_blocking()`
- Ce `wait_blocking()` bloque le thread scheduler actuel
- Le `SegmentUpdaterActor` doit traiter le `AddSegment` message sur un AUTRE thread scheduler
- Si `num_worker_threads == 1`, il n'y a qu'un seul thread → deadlock !

**Fix potentiel** : utiliser `wait_cooperative(|| scheduler.run_one_step())` au lieu
de `wait_blocking()` pour les appels depuis le thread scheduler. Cela nécessite de
passer le scheduler aux endroits concernés, ou de détecter si on est sur un thread
scheduler.

Alternative : augmenter `num_worker_threads` pour les tests (mais ça ne résout pas
le cas single-thread de la Phase 4).
