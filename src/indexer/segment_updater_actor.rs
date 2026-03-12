use std::collections::{HashSet, VecDeque};
use std::sync::Arc;

use std::time::Instant;

use crate::actor::{Actor, ActorRef, ActorStatus, Priority};
use crate::actor::Reply;
use crate::indexer::events::IndexEvent;
use crate::directory::GarbageCollectionResult;
use crate::index::{SegmentId, SegmentMeta};
use crate::indexer::merge_operation::MergeOperation;
use crate::indexer::merge_state::{MergeState, StepResult};
use crate::indexer::segment_updater::{garbage_collect_files, SegmentUpdaterShared};
use crate::indexer::SegmentEntry;

/// Messages reçus par le SegmentUpdaterActor.
pub(crate) enum SegmentUpdaterMsg {
    /// Un nouveau segment a été finalisé par un IndexerActor.
    /// Fire-and-forget : pas de reply (appelé depuis un handler d'acteur,
    /// bloquer provoquerait un deadlock).
    AddSegment {
        entry: SegmentEntry,
    },
    /// Commit : purge deletes + save metas + GC + consider merges.
    Commit {
        opstamp: crate::Opstamp,
        payload: Option<String>,
        reply: Reply<crate::Result<crate::Opstamp>>,
    },
    /// Garbage collect les fichiers obsolètes.
    GarbageCollect(Reply<crate::Result<GarbageCollectionResult>>),
    /// Démarre un merge explicite (appelé depuis IndexWriter::merge()).
    StartMerge {
        merge_operation: MergeOperation,
        reply: Reply<crate::Result<Option<SegmentMeta>>>,
    },
    /// Avance d'un step le merge en cours (self-message).
    /// Passe par la mailbox normale → le scheduler intercale avec les autres
    /// messages et les autres acteurs. Résout le problème de monopolisation
    /// des threads par poll_idle.
    MergeStep,
    /// Attend que tous les merges incrémentaux en cours/en attente soient terminés.
    DrainMerges(Reply<()>),
    /// Arrêt propre.
    Kill,
}

/// Merge incrémental en cours d'exécution.
struct ActiveMerge {
    merge_operation: MergeOperation,
    state: MergeState,
    start_time: Instant,
}

/// Merge explicite (IndexWriter::merge) en cours d'exécution.
/// Prioritaire sur les merges automatiques.
struct ExplicitMerge {
    merge_operation: MergeOperation,
    state: MergeState,
    start_time: Instant,
    reply: Reply<crate::Result<Option<SegmentMeta>>>,
}

/// Acteur gérant les mises à jour de segments.
///
/// Les merges (automatiques et explicites) s'exécutent de manière incrémentale
/// via des self-messages `MergeStep`. Chaque step passe par la mailbox normale,
/// ce qui permet au scheduler d'intercaler les messages des autres acteurs
/// (Flush, Commit, etc.) entre chaque step de merge.
pub(crate) struct SegmentUpdaterActor {
    shared: Arc<SegmentUpdaterShared>,
    /// Référence à soi-même pour les self-messages (MergeStep).
    /// Initialisé dans on_start() après que le wake_handle soit attaché.
    self_ref: Option<ActorRef<SegmentUpdaterMsg>>,
    /// Merge incrémental en cours (au plus un à la fois).
    active_merge: Option<ActiveMerge>,
    /// Merge explicite en cours (prioritaire sur les merges auto).
    explicit_merge: Option<ExplicitMerge>,
    /// File d'attente des merges automatiques à exécuter.
    pending_merges: VecDeque<MergeOperation>,
    /// Segments actuellement en cours de merge (actif + pending + explicit).
    /// Single source of truth — remplace census::Inventory.
    segments_in_merge: HashSet<SegmentId>,
}

impl SegmentUpdaterActor {
    pub fn new(shared: Arc<SegmentUpdaterShared>) -> Self {
        SegmentUpdaterActor {
            shared,
            self_ref: None,
            active_merge: None,
            explicit_merge: None,
            pending_merges: VecDeque::new(),
            segments_in_merge: HashSet::new(),
        }
    }

    /// S'envoie un MergeStep si un merge est en cours.
    fn schedule_merge_step(&self) {
        if self.active_merge.is_some() || self.explicit_merge.is_some() {
            if let Some(ref self_ref) = self.self_ref {
                let _ = self_ref.send(SegmentUpdaterMsg::MergeStep);
            }
        }
    }

    /// Avance d'un step le merge en cours.
    /// Merge explicite prioritaire, puis merge auto.
    fn handle_merge_step(&mut self) {
        // 1. Merge explicite — prioritaire.
        if let Some(mut explicit) = self.explicit_merge.take() {
            match explicit.state.step() {
                StepResult::Continue => {
                    self.emit_step_completed(&explicit.merge_operation, &explicit.state);
                    self.explicit_merge = Some(explicit);
                    self.schedule_merge_step();
                }
                StepResult::Done(result) => {
                    self.finish_explicit_merge(explicit, result);
                    // S'il reste un merge auto, continuer.
                    self.schedule_merge_step();
                }
            }
            return;
        }

        // 2. Merge incrémental (automatique).
        let active = match self.active_merge.take() {
            Some(a) => a,
            None => return,
        };

        let ActiveMerge { merge_operation, mut state, start_time } = active;

        match state.step() {
            StepResult::Continue => {
                self.emit_step_completed(&merge_operation, &state);
                self.active_merge = Some(ActiveMerge {
                    merge_operation,
                    state,
                    start_time,
                });
                self.schedule_merge_step();
            }
            StepResult::Done(result) => {
                let active_done = ActiveMerge { merge_operation, state, start_time };
                self.finish_incremental_merge(active_done, result);
                // finish_incremental_merge peut démarrer un nouveau merge.
                self.schedule_merge_step();
            }
        }
    }

    fn handle_add_segment(&mut self, entry: SegmentEntry) {
        self.shared.segment_manager.add_segment(entry);
        self.enqueue_merge_candidates();
    }

    fn handle_commit(
        &mut self,
        opstamp: crate::Opstamp,
        payload: Option<String>,
        reply: Reply<crate::Result<crate::Opstamp>>,
    ) {
        let start = Instant::now();
        self.shared.event_bus.emit(IndexEvent::CommitStarted { opstamp });
        let result = (|| {
            let segment_entries = self.shared.purge_deletes(opstamp)?;
            self.shared.segment_manager.commit(segment_entries);
            self.shared.save_metas(opstamp, payload)?;
            let _ = garbage_collect_files(&self.shared);
            self.enqueue_merge_candidates();
            Ok(opstamp)
        })();
        self.shared.event_bus.emit(IndexEvent::CommitCompleted {
            opstamp,
            duration: start.elapsed(),
        });
        reply.send(result);
    }

    fn handle_garbage_collect(
        &mut self,
        reply: Reply<crate::Result<GarbageCollectionResult>>,
    ) {
        let result = garbage_collect_files(&self.shared);
        reply.send(result);
    }

    /// Merge explicite (IndexWriter::merge) — non-blocking.
    /// Le merge s'exécute en steps via self-messages MergeStep.
    fn handle_start_merge(
        &mut self,
        merge_operation: MergeOperation,
        reply: Reply<crate::Result<Option<SegmentMeta>>>,
    ) {
        assert!(
            !merge_operation.segment_ids().is_empty(),
            "Segment_ids cannot be empty."
        );

        // Si un merge explicite est déjà en cours, on refuse (un seul à la fois).
        if self.explicit_merge.is_some() {
            reply.send(Err(crate::LucivyError::SystemError(
                "An explicit merge is already in progress".to_string(),
            )));
            return;
        }

        let segment_entries = match self
            .shared
            .segment_manager
            .start_merge(merge_operation.segment_ids())
        {
            Ok(entries) => entries,
            Err(err) => {
                warn!(
                    "Starting the merge failed for the following reason. This is not fatal. {err}"
                );
                reply.send(Err(err));
                return;
            }
        };

        info!("Starting merge (explicit) - {:?}", merge_operation.segment_ids());
        self.segments_in_merge.extend(merge_operation.segment_ids());
        self.shared.event_bus.emit(IndexEvent::MergeStarted {
            segment_ids: merge_operation.segment_ids().to_vec(),
            target_opstamp: merge_operation.target_opstamp(),
        });

        let index = &self.shared.index;
        let target_opstamp = merge_operation.target_opstamp();

        match MergeState::new(index, segment_entries, target_opstamp) {
            Ok(Some(state)) => {
                self.explicit_merge = Some(ExplicitMerge {
                    merge_operation,
                    state,
                    start_time: Instant::now(),
                    reply,
                });
                self.schedule_merge_step();
            }
            Ok(None) => {
                // Tous les segments sont vides — end_merge directement.
                self.shared.event_bus.emit(IndexEvent::MergeCompleted {
                    segment_ids: merge_operation.segment_ids().to_vec(),
                    duration: std::time::Duration::ZERO,
                    result_num_docs: 0,
                });
                self.untrack_segments(&merge_operation);
                let result = self.do_end_merge(merge_operation, None)
                    .map(|()| None);
                reply.send(result);
            }
            Err(e) => {
                self.shared.event_bus.emit(IndexEvent::MergeFailed {
                    segment_ids: merge_operation.segment_ids().to_vec(),
                    error: format!("{e:?}"),
                });
                self.untrack_segments(&merge_operation);
                reply.send(Err(e));
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

    // --- Merge incrémental (automatique) ---

    /// Collecte les candidats au merge et les ajoute à la file d'attente.
    /// Si aucun merge n'est en cours, démarre le premier candidat.
    fn enqueue_merge_candidates(&mut self) {
        let candidates = self.collect_merge_candidates();
        for op in candidates {
            self.pending_merges.push_back(op);
        }
        if self.active_merge.is_none() {
            self.start_next_incremental_merge();
        }
    }

    /// Démarre le prochain merge incrémental de la file d'attente.
    fn start_next_incremental_merge(&mut self) {
        while let Some(merge_op) = self.pending_merges.pop_front() {
            assert!(
                !merge_op.segment_ids().is_empty(),
                "Segment_ids cannot be empty."
            );

            let segment_entries = match self
                .shared
                .segment_manager
                .start_merge(merge_op.segment_ids())
            {
                Ok(entries) => entries,
                Err(err) => {
                    warn!("Starting incremental merge failed (not fatal): {err}");
                    continue;
                }
            };

            info!("Starting merge (incremental) - {:?}", merge_op.segment_ids());
            self.segments_in_merge.extend(merge_op.segment_ids());
            self.shared.event_bus.emit(IndexEvent::MergeStarted {
                segment_ids: merge_op.segment_ids().to_vec(),
                target_opstamp: merge_op.target_opstamp(),
            });

            let index = &self.shared.index;
            let target_opstamp = merge_op.target_opstamp();

            match MergeState::new(index, segment_entries, target_opstamp) {
                Ok(Some(state)) => {
                    self.active_merge = Some(ActiveMerge {
                        merge_operation: merge_op,
                        state,
                        start_time: Instant::now(),
                    });
                    self.schedule_merge_step();
                    return;
                }
                Ok(None) => {
                    // Tous les segments sont vides — end_merge directement.
                    self.untrack_segments(&merge_op);
                    if let Err(e) = self.do_end_merge(merge_op, None) {
                        warn!("End merge (empty) failed: {e:?}");
                    }
                    continue;
                }
                Err(e) => {
                    warn!("Creating incremental merge state failed: {e:?}");
                    self.shared.event_bus.emit(IndexEvent::MergeFailed {
                        segment_ids: merge_op.segment_ids().to_vec(),
                        error: format!("{e:?}"),
                    });
                    self.untrack_segments(&merge_op);
                    if cfg!(test) {
                        panic!("Incremental merge state creation failed: {e:?}");
                    }
                    continue;
                }
            }
        }
    }

    /// Appelé quand un merge explicite se termine.
    /// Envoie le résultat via la reply.
    fn finish_explicit_merge(&mut self, explicit: ExplicitMerge, result: Option<SegmentEntry>) {
        let result_num_docs = result
            .as_ref()
            .map(|e| e.meta().num_docs())
            .unwrap_or(0);
        let after_merge_meta = result.as_ref().map(|e| e.meta().clone());
        self.shared.event_bus.emit(IndexEvent::MergeCompleted {
            segment_ids: explicit.merge_operation.segment_ids().to_vec(),
            duration: explicit.start_time.elapsed(),
            result_num_docs,
        });
        self.untrack_segments(&explicit.merge_operation);
        match self.do_end_merge(explicit.merge_operation, result) {
            Ok(()) => explicit.reply.send(Ok(after_merge_meta)),
            Err(e) => explicit.reply.send(Err(e)),
        }
    }

    /// Appelé quand un merge incrémental se termine (succès ou échec).
    /// Finalise le merge et relance la recherche de candidats.
    fn finish_incremental_merge(&mut self, active: ActiveMerge, result: Option<SegmentEntry>) {
        let result_num_docs = result
            .as_ref()
            .map(|e| e.meta().num_docs())
            .unwrap_or(0);
        self.shared.event_bus.emit(IndexEvent::MergeCompleted {
            segment_ids: active.merge_operation.segment_ids().to_vec(),
            duration: active.start_time.elapsed(),
            result_num_docs,
        });
        self.untrack_segments(&active.merge_operation);
        if let Err(e) = self.do_end_merge(active.merge_operation, result) {
            warn!("End merge (incremental) failed: {e:?}");
        }
        // Après un merge, l'état des segments a changé — re-collecter.
        let new_candidates = self.collect_merge_candidates();
        for op in new_candidates {
            self.pending_merges.push_back(op);
        }
        self.start_next_incremental_merge();
    }

    /// Exécute tous les merges en cours et en attente de manière blocking.
    /// Appelé par DrainMerges pour que wait_merging_threads fonctionne.
    fn drain_all_merges(&mut self) {
        // 0. Finir le merge explicite s'il y en a un.
        if let Some(explicit) = self.explicit_merge.take() {
            let ExplicitMerge { merge_operation, mut state, start_time, reply } = explicit;
            loop {
                match state.step() {
                    StepResult::Continue => continue,
                    StepResult::Done(result) => {
                        let result_num_docs = result
                            .as_ref()
                            .map(|e| e.meta().num_docs())
                            .unwrap_or(0);
                        let after_merge_meta = result.as_ref().map(|e| e.meta().clone());
                        self.shared.event_bus.emit(IndexEvent::MergeCompleted {
                            segment_ids: merge_operation.segment_ids().to_vec(),
                            duration: start_time.elapsed(),
                            result_num_docs,
                        });
                        self.untrack_segments(&merge_operation);
                        match self.do_end_merge(merge_operation, result) {
                            Ok(()) => reply.send(Ok(after_merge_meta)),
                            Err(e) => reply.send(Err(e)),
                        }
                        break;
                    }
                }
            }
        }

        // 1. Finir le merge actif s'il y en a un.
        if let Some(active) = self.active_merge.take() {
            let ActiveMerge { merge_operation, mut state, start_time } = active;
            loop {
                match state.step() {
                    StepResult::Continue => continue,
                    StepResult::Done(result) => {
                        let result_num_docs = result
                            .as_ref()
                            .map(|e| e.meta().num_docs())
                            .unwrap_or(0);
                        self.shared.event_bus.emit(IndexEvent::MergeCompleted {
                            segment_ids: merge_operation.segment_ids().to_vec(),
                            duration: start_time.elapsed(),
                            result_num_docs,
                        });
                        self.untrack_segments(&merge_operation);
                        if let Err(e) = self.do_end_merge(merge_operation, result) {
                            warn!("End merge (drain) failed: {e:?}");
                        }
                        break;
                    }
                }
            }
        }

        // 2. Exécuter tous les merges en attente.
        while let Some(merge_op) = self.pending_merges.pop_front() {
            let segment_entries = match self
                .shared
                .segment_manager
                .start_merge(merge_op.segment_ids())
            {
                Ok(entries) => entries,
                Err(err) => {
                    warn!("Starting drain merge failed (not fatal): {err}");
                    continue;
                }
            };

            self.segments_in_merge.extend(merge_op.segment_ids());
            self.shared.event_bus.emit(IndexEvent::MergeStarted {
                segment_ids: merge_op.segment_ids().to_vec(),
                target_opstamp: merge_op.target_opstamp(),
            });
            let drain_start = Instant::now();
            let index = &self.shared.index;
            let target_opstamp = merge_op.target_opstamp();

            match MergeState::new(index, segment_entries, target_opstamp) {
                Ok(Some(mut state)) => {
                    loop {
                        match state.step() {
                            StepResult::Continue => continue,
                            StepResult::Done(result) => {
                                let result_num_docs = result
                                    .as_ref()
                                    .map(|e| e.meta().num_docs())
                                    .unwrap_or(0);
                                self.shared.event_bus.emit(IndexEvent::MergeCompleted {
                                    segment_ids: merge_op.segment_ids().to_vec(),
                                    duration: drain_start.elapsed(),
                                    result_num_docs,
                                });
                                self.untrack_segments(&merge_op);
                                if let Err(e) = self.do_end_merge(merge_op, result) {
                                    warn!("End merge (drain) failed: {e:?}");
                                }
                                break;
                            }
                        }
                    }
                }
                Ok(None) => {
                    self.shared.event_bus.emit(IndexEvent::MergeCompleted {
                        segment_ids: merge_op.segment_ids().to_vec(),
                        duration: drain_start.elapsed(),
                        result_num_docs: 0,
                    });
                    self.untrack_segments(&merge_op);
                    if let Err(e) = self.do_end_merge(merge_op, None) {
                        warn!("End merge (drain/empty) failed: {e:?}");
                    }
                }
                Err(e) => {
                    self.shared.event_bus.emit(IndexEvent::MergeFailed {
                        segment_ids: merge_op.segment_ids().to_vec(),
                        error: format!("{e:?}"),
                    });
                    self.untrack_segments(&merge_op);
                    warn!("Creating drain merge state failed: {e:?}");
                }
            }
        }
    }

    /// Émet un event MergeStepCompleted (si des subscribers écoutent).
    fn emit_step_completed(&self, merge_op: &MergeOperation, state: &MergeState) {
        self.shared.event_bus.emit(IndexEvent::MergeStepCompleted {
            segment_ids: merge_op.segment_ids().to_vec(),
            steps_completed: state.steps_completed(),
            steps_total: state.estimated_steps(),
        });
    }

    /// Retire les segments d'une merge operation du tracking.
    fn untrack_segments(&mut self, merge_op: &MergeOperation) {
        for segment_id in merge_op.segment_ids() {
            self.segments_in_merge.remove(segment_id);
        }
    }

    fn collect_merge_candidates(&self) -> Vec<MergeOperation> {
        let (mut committed_segments, mut uncommitted_segments) =
            self.shared.get_mergeable_segments(&self.segments_in_merge);
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
                MergeOperation::new(current_opstamp, merge_candidate.0)
            })
            .collect();

        let commit_opstamp = self.shared.load_meta().opstamp;
        let committed_merge_candidates = merge_policy
            .compute_merge_candidates(&committed_segments)
            .into_iter()
            .map(|merge_candidate| {
                MergeOperation::new(commit_opstamp, merge_candidate.0)
            });
        merge_candidates.extend(committed_merge_candidates);

        // Filtrer les merges d'un seul segment sans deletes — c'est un no-op
        // qui provoquerait une boucle infinie en mode synchrone.
        merge_candidates.retain(|op| op.segment_ids().len() > 1);

        merge_candidates
    }
}

impl Actor for SegmentUpdaterActor {
    type Msg = SegmentUpdaterMsg;

    fn name(&self) -> &'static str {
        "segment_updater"
    }

    fn on_start(&mut self, self_ref: ActorRef<SegmentUpdaterMsg>) {
        self.self_ref = Some(self_ref);
    }

    fn handle(&mut self, msg: SegmentUpdaterMsg) -> ActorStatus {
        match msg {
            SegmentUpdaterMsg::AddSegment { entry } => {
                self.handle_add_segment(entry);
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
            SegmentUpdaterMsg::MergeStep => {
                self.handle_merge_step();
                ActorStatus::Continue
            }
            SegmentUpdaterMsg::DrainMerges(reply) => {
                self.drain_all_merges();
                reply.send(());
                ActorStatus::Continue
            }
            SegmentUpdaterMsg::Kill => ActorStatus::Stop,
        }
    }

    fn priority(&self) -> Priority {
        Priority::Medium
    }
}
