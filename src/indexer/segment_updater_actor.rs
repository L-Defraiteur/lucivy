use std::any::Any;
use std::sync::Arc;

use crate::actor::{Actor, ActorStatus, Priority};
use crate::actor::Reply;
use crate::directory::GarbageCollectionResult;
use crate::index::SegmentMeta;
use crate::indexer::merge_operation::MergeOperation;
use crate::indexer::segment_updater::{garbage_collect_files, merge, SegmentUpdaterShared};
use crate::indexer::SegmentEntry;

/// Messages reçus par le SegmentUpdaterActor.
pub(crate) enum SegmentUpdaterMsg {
    /// Un nouveau segment a été finalisé par un IndexerActor.
    AddSegment {
        entry: SegmentEntry,
        reply: Reply<crate::Result<()>>,
    },
    /// Commit : purge deletes + save metas + GC + consider merges.
    Commit {
        opstamp: crate::Opstamp,
        payload: Option<String>,
        reply: Reply<crate::Result<crate::Opstamp>>,
    },
    /// Garbage collect les fichiers obsolètes.
    GarbageCollect(Reply<crate::Result<GarbageCollectionResult>>),
    /// Démarre un merge (appelé depuis IndexWriter::merge()).
    StartMerge {
        merge_operation: MergeOperation,
        reply: Reply<crate::Result<Option<SegmentMeta>>>,
    },
    /// Arrêt propre.
    Kill,
}

/// Acteur gérant les mises à jour de segments.
///
/// Toutes les opérations séquentielles (add segment, commit, merge, GC)
/// passent par la mailbox FIFO de cet acteur. Les merges s'exécutent
/// directement dans le handler (blocking) — le scheduler alloue d'autres
/// threads aux autres acteurs pendant ce temps.
pub(crate) struct SegmentUpdaterActor {
    shared: Arc<SegmentUpdaterShared>,
}

impl SegmentUpdaterActor {
    pub fn new(shared: Arc<SegmentUpdaterShared>) -> Self {
        SegmentUpdaterActor { shared }
    }

    fn handle_add_segment(&mut self, entry: SegmentEntry, reply: Reply<crate::Result<()>>) {
        self.shared.segment_manager.add_segment(entry);
        self.consider_merge_options();
        reply.send(Ok(()));
    }

    fn handle_commit(
        &mut self,
        opstamp: crate::Opstamp,
        payload: Option<String>,
        reply: Reply<crate::Result<crate::Opstamp>>,
    ) {
        let result = (|| {
            let segment_entries = self.shared.purge_deletes(opstamp)?;
            self.shared.segment_manager.commit(segment_entries);
            self.shared.save_metas(opstamp, payload)?;
            let _ = garbage_collect_files(&self.shared);
            self.consider_merge_options();
            Ok(opstamp)
        })();
        reply.send(result);
    }

    fn handle_garbage_collect(
        &mut self,
        reply: Reply<crate::Result<GarbageCollectionResult>>,
    ) {
        let result = garbage_collect_files(&self.shared);
        reply.send(result);
    }

    fn handle_start_merge(
        &mut self,
        merge_operation: MergeOperation,
        reply: Reply<crate::Result<Option<SegmentMeta>>>,
    ) {
        let result = self.run_merge(merge_operation);
        reply.send(result);
    }

    /// Exécute un merge de manière synchrone (blocking).
    /// Remplace l'ancien spawn sur rayon — tout reste dans le scheduler.
    fn run_merge(
        &mut self,
        merge_operation: MergeOperation,
        ) -> crate::Result<Option<SegmentMeta>> {
        assert!(
            !merge_operation.segment_ids().is_empty(),
            "Segment_ids cannot be empty."
        );

        let segment_entries: Vec<SegmentEntry> = match self
            .shared
            .segment_manager
            .start_merge(merge_operation.segment_ids())
        {
            Ok(entries) => entries,
            Err(err) => {
                warn!(
                    "Starting the merge failed for the following reason. This is not fatal. {err}"
                );
                return Err(err);
            }
        };

        info!("Starting merge  - {:?}", merge_operation.segment_ids());

        let index = &self.shared.index;
        let target_opstamp = merge_operation.target_opstamp();

        // Exécute le merge inline (blocking). Avec N threads scheduler,
        // les autres acteurs continuent sur les N-1 threads restants.
        let merge_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            merge(index, segment_entries, target_opstamp)
        }));

        match merge_result {
            Ok(Ok(after_merge_entry)) => {
                let after_merge_meta = after_merge_entry
                    .as_ref()
                    .map(|e| e.meta().clone());
                self.do_end_merge(merge_operation, after_merge_entry)?;
                Ok(after_merge_meta)
            }
            Ok(Err(merge_error)) => {
                warn!(
                    "Merge of {:?} was cancelled: {:?}",
                    merge_operation.segment_ids().to_vec(),
                    merge_error
                );
                if cfg!(test) {
                    panic!("{merge_error:?}");
                }
                Err(merge_error)
            }
            Err(panic_err) => {
                let panic_str = extract_panic_message(&panic_err);
                error!("Merge panicked: {panic_str}");
                Err(crate::LucivyError::SystemError(format!(
                    "Merge panicked: {panic_str}"
                )))
            }
        }
    }

    /// Logique de end_merge — portée depuis InnerSegmentUpdater::end_merge.
    fn do_end_merge(
        &mut self,
        merge_operation: MergeOperation,
        mut after_merge_segment_entry: Option<SegmentEntry>,
    ) -> crate::Result<()> {
        info!(
            "End merge {:?}",
            after_merge_segment_entry.as_ref().map(|entry| entry.meta())
        );

        if let Some(after_merge_segment_entry) = after_merge_segment_entry.as_mut() {
            let mut delete_cursor = after_merge_segment_entry.delete_cursor().clone();
            if let Some(delete_operation) = delete_cursor.get() {
                let committed_opstamp = self.shared.load_meta().opstamp;
                if delete_operation.opstamp < committed_opstamp {
                    let index = &self.shared.index;
                    let segment = index.segment(after_merge_segment_entry.meta().clone());
                    if let Err(advance_deletes_err) =
                        crate::indexer::index_writer::advance_deletes(
                            segment,
                            after_merge_segment_entry,
                            committed_opstamp,
                        )
                    {
                        error!(
                            "Merge of {:?} was cancelled (advancing deletes failed): {:?}",
                            merge_operation.segment_ids(),
                            advance_deletes_err
                        );
                        assert!(!cfg!(test), "Merge failed.");
                        return Err(advance_deletes_err);
                    }
                }
            }
        }

        let previous_metas = self.shared.load_meta();
        let segments_status = self.shared.segment_manager.end_merge(
            merge_operation.segment_ids(),
            after_merge_segment_entry,
        )?;

        if segments_status == crate::indexer::segment_manager::SegmentsStatus::Committed {
            self.shared
                .save_metas(previous_metas.opstamp, previous_metas.payload.clone())?;
        }

        let _ = garbage_collect_files(&self.shared);
        Ok(())
    }

    /// Collecte les candidats au merge et les exécute en boucle
    /// jusqu'à ce qu'il n'y ait plus de candidats.
    /// Itératif (pas de récursion via do_end_merge).
    fn consider_merge_options(&mut self) {
        loop {
            let candidates = self.collect_merge_candidates();
            if candidates.is_empty() {
                break;
            }
            for merge_operation in candidates {
                if let Err(e) = self.run_merge(merge_operation) {
                    warn!("Automatic merge failed: {e:?}");
                }
            }
        }
    }

    fn collect_merge_candidates(&self) -> Vec<MergeOperation> {
        let (mut committed_segments, mut uncommitted_segments) =
            self.shared.get_mergeable_segments();
        if committed_segments.len() == 1 && committed_segments[0].num_deleted_docs() == 0 {
            committed_segments.clear();
        }
        if uncommitted_segments.len() == 1 && uncommitted_segments[0].num_deleted_docs() == 0 {
            uncommitted_segments.clear();
        }

        let merge_policy = self.shared.get_merge_policy();
        let current_opstamp = self.shared.stamper.stamp();

        let mut merge_candidates: Vec<MergeOperation> = merge_policy
            .compute_merge_candidates(&uncommitted_segments)
            .into_iter()
            .map(|merge_candidate| {
                MergeOperation::new(
                    &self.shared.merge_operations,
                    current_opstamp,
                    merge_candidate.0,
                )
            })
            .collect();

        let commit_opstamp = self.shared.load_meta().opstamp;
        let committed_merge_candidates = merge_policy
            .compute_merge_candidates(&committed_segments)
            .into_iter()
            .map(|merge_candidate| {
                MergeOperation::new(
                    &self.shared.merge_operations,
                    commit_opstamp,
                    merge_candidate.0,
                )
            });
        merge_candidates.extend(committed_merge_candidates);

        // Filtrer les merges d'un seul segment sans deletes — c'est un no-op
        // qui provoquerait une boucle infinie en mode synchrone.
        // Les merges d'un seul segment AVEC des deletes restent utiles (compaction).
        merge_candidates.retain(|op| op.segment_ids().len() > 1);

        merge_candidates
    }
}

impl Actor for SegmentUpdaterActor {
    type Msg = SegmentUpdaterMsg;

    fn name(&self) -> &'static str {
        "segment_updater"
    }

    fn handle(&mut self, msg: SegmentUpdaterMsg) -> ActorStatus {
        match msg {
            SegmentUpdaterMsg::AddSegment { entry, reply } => {
                self.handle_add_segment(entry, reply);
                ActorStatus::Continue
            }
            SegmentUpdaterMsg::Commit {
                opstamp,
                payload,
                reply,
            } => {
                self.handle_commit(opstamp, payload, reply);
                ActorStatus::Continue
            }
            SegmentUpdaterMsg::GarbageCollect(reply) => {
                self.handle_garbage_collect(reply);
                ActorStatus::Continue
            }
            SegmentUpdaterMsg::StartMerge {
                merge_operation,
                reply,
            } => {
                self.handle_start_merge(merge_operation, reply);
                ActorStatus::Continue
            }
            SegmentUpdaterMsg::Kill => ActorStatus::Stop,
        }
    }

    fn priority(&self) -> Priority {
        Priority::Medium
    }
}

fn extract_panic_message(panic_err: &Box<dyn Any + Send>) -> &str {
    if let Some(msg) = panic_err.downcast_ref::<&str>() {
        msg
    } else if let Some(msg) = panic_err.downcast_ref::<String>() {
        msg.as_str()
    } else {
        "UNKNOWN"
    }
}
