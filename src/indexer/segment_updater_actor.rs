//! SegmentUpdaterActor — segment management via GenericActor.
//!
//! Handles: AddSegment, Commit, GarbageCollect, StartMerge, MergeStep, DrainMerges, Kill.
//! Merges run incrementally via self-messages (MergeStep).

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::time::Instant;

use crate::actor::actor_state::ActorState;
use crate::actor::envelope::{type_tag_hash, Envelope, Message, ReplyPort};
use crate::actor::generic_actor::GenericActor;
use crate::actor::handler::TypedHandler;
use crate::actor::mailbox::ActorRef;
use crate::actor::{ActorStatus, Priority};
use crate::directory::GarbageCollectionResult;
use crate::index::{SegmentId, SegmentMeta};
use crate::indexer::events::IndexEvent;
use crate::indexer::merge_operation::MergeOperation;
use crate::indexer::merge_state::{MergeState, StepResult};
use crate::indexer::segment_updater::{garbage_collect_files, SegmentUpdaterShared};
use crate::indexer::SegmentEntry;
use crate::LucivyError;

// ─── Messages ───────────────────────────────────────────────────────────────

pub(crate) struct SuAddSegmentMsg;
impl Message for SuAddSegmentMsg {
    fn type_tag() -> u64 { type_tag_hash(b"SuAddSegmentMsg") }
    fn encode(&self) -> Vec<u8> { vec![] }
    fn decode(_: &[u8]) -> Result<Self, String> { Ok(Self) }
}

pub(crate) struct SuCommitMsg {
    pub opstamp: crate::Opstamp,
    pub payload: Option<String>,
    /// If true, rebuild suffix FST for deferred segments after commit.
    pub rebuild_sfx: bool,
}
impl Message for SuCommitMsg {
    fn type_tag() -> u64 { type_tag_hash(b"SuCommitMsg") }
    fn encode(&self) -> Vec<u8> {
        let mut buf = self.opstamp.to_le_bytes().to_vec();
        match &self.payload {
            Some(p) => { buf.push(1); buf.extend_from_slice(p.as_bytes()); }
            None => { buf.push(0); }
        }
        buf.push(if self.rebuild_sfx { 1 } else { 0 });
        buf
    }
    fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() < 9 { return Err("too short".into()); }
        let opstamp = u64::from_le_bytes(bytes[..8].try_into().unwrap());
        let payload = if bytes[8] == 1 {
            Some(String::from_utf8_lossy(&bytes[9..bytes.len() - 1]).to_string())
        } else { None };
        let rebuild_sfx = bytes.last().copied().unwrap_or(1) == 1;
        Ok(Self { opstamp, payload, rebuild_sfx })
    }
}

pub(crate) struct SuGarbageCollectMsg;
impl Message for SuGarbageCollectMsg {
    fn type_tag() -> u64 { type_tag_hash(b"SuGarbageCollectMsg") }
    fn encode(&self) -> Vec<u8> { vec![] }
    fn decode(_: &[u8]) -> Result<Self, String> { Ok(Self) }
}

pub(crate) struct SuStartMergeMsg;
impl Message for SuStartMergeMsg {
    fn type_tag() -> u64 { type_tag_hash(b"SuStartMergeMsg") }
    fn encode(&self) -> Vec<u8> { vec![] }
    fn decode(_: &[u8]) -> Result<Self, String> { Ok(Self) }
}

pub(crate) struct SuMergeStepMsg;
impl Message for SuMergeStepMsg {
    fn type_tag() -> u64 { type_tag_hash(b"SuMergeStepMsg") }
    fn encode(&self) -> Vec<u8> { vec![] }
    fn decode(_: &[u8]) -> Result<Self, String> { Ok(Self) }
}

pub(crate) struct SuDrainMergesMsg;
impl Message for SuDrainMergesMsg {
    fn type_tag() -> u64 { type_tag_hash(b"SuDrainMergesMsg") }
    fn encode(&self) -> Vec<u8> { vec![] }
    fn decode(_: &[u8]) -> Result<Self, String> { Ok(Self) }
}

pub(crate) struct SuKillMsg;
impl Message for SuKillMsg {
    fn type_tag() -> u64 { type_tag_hash(b"SuKillMsg") }
    fn encode(&self) -> Vec<u8> { vec![] }
    fn decode(_: &[u8]) -> Result<Self, String> { Ok(Self) }
}

// Reply messages
pub(crate) struct SuOkReply;
impl Message for SuOkReply {
    fn type_tag() -> u64 { type_tag_hash(b"SuOkReply") }
    fn encode(&self) -> Vec<u8> { vec![] }
    fn decode(_: &[u8]) -> Result<Self, String> { Ok(Self) }
}

pub(crate) struct SuOpsReply {
    pub opstamp: crate::Opstamp,
}
impl Message for SuOpsReply {
    fn type_tag() -> u64 { type_tag_hash(b"SuOpsReply") }
    fn encode(&self) -> Vec<u8> { self.opstamp.to_le_bytes().to_vec() }
    fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() < 8 { return Err("too short".into()); }
        Ok(Self { opstamp: u64::from_le_bytes(bytes[..8].try_into().unwrap()) })
    }
}

// ─── State ──────────────────────────────────────────────────────────────────

struct ActiveMerge {
    merge_operation: MergeOperation,
    state: MergeState,
    start_time: Instant,
}

struct ExplicitMerge {
    merge_operation: MergeOperation,
    state: MergeState,
    start_time: Instant,
    reply: ReplyPort,
}

pub(crate) struct SegmentUpdaterState {
    shared: Arc<SegmentUpdaterShared>,
    active_merge: Option<ActiveMerge>,
    explicit_merge: Option<ExplicitMerge>,
    pending_merges: VecDeque<MergeOperation>,
    segments_in_merge: HashSet<SegmentId>,
}

impl SegmentUpdaterState {
    fn schedule_merge_step(&self, state: &ActorState) {
        if self.active_merge.is_some() || self.explicit_merge.is_some() {
            if let Some(self_ref) = state.get::<ActorRef<Envelope>>() {
                let _ = self_ref.send(SuMergeStepMsg.into_envelope());
            }
        }
    }

    fn handle_merge_step(&mut self, actor_state: &ActorState) {
        if let Some(mut explicit) = self.explicit_merge.take() {
            match explicit.state.step() {
                StepResult::Continue => {
                    self.emit_step_completed(&explicit.merge_operation, &explicit.state);
                    self.explicit_merge = Some(explicit);
                    self.schedule_merge_step(actor_state);
                }
                StepResult::Done(result) => {
                    self.finish_explicit_merge(explicit, result);
                    self.schedule_merge_step(actor_state);
                }
            }
            return;
        }

        let active = match self.active_merge.take() {
            Some(a) => a,
            None => return,
        };

        let ActiveMerge { merge_operation, mut state, start_time } = active;
        match state.step() {
            StepResult::Continue => {
                self.emit_step_completed(&merge_operation, &state);
                self.active_merge = Some(ActiveMerge { merge_operation, state, start_time });
                self.schedule_merge_step(actor_state);
            }
            StepResult::Done(result) => {
                let active_done = ActiveMerge { merge_operation, state, start_time };
                self.finish_incremental_merge(active_done, result, actor_state);
                self.schedule_merge_step(actor_state);
            }
        }
    }

    fn handle_add_segment(&mut self, entry: SegmentEntry, actor_state: &ActorState) {
        self.shared.segment_manager.add_segment(entry);
        self.enqueue_merge_candidates(actor_state);
    }

    fn handle_commit(
        &mut self,
        opstamp: crate::Opstamp,
        payload: Option<String>,
        rebuild_sfx: bool,
    ) -> crate::Result<crate::Opstamp> {
        let start = Instant::now();
        self.shared.event_bus.emit(IndexEvent::CommitStarted { opstamp });
        let result = if rebuild_sfx {
            self.handle_commit_dag(opstamp, payload)
        } else {
            self.handle_commit_fast(opstamp, payload)
        };
        self.shared.event_bus.emit(IndexEvent::CommitCompleted {
            opstamp,
            duration: start.elapsed(),
        });
        result
    }

    /// Fast commit path: no merges, just purge + save + optional GC.
    fn handle_commit_fast(
        &mut self,
        opstamp: crate::Opstamp,
        payload: Option<String>,
    ) -> crate::Result<crate::Opstamp> {
        let segment_entries = self.shared.purge_deletes(opstamp)?;
        self.shared.segment_manager.commit(segment_entries);
        self.shared.save_metas(opstamp, payload)?;
        if self.segments_in_merge.is_empty() {
            let _ = garbage_collect_files(&self.shared);
        }
        Ok(opstamp)
    }

    /// Full commit via DAG: drain active merges, collect ALL merge candidates,
    /// execute them in parallel, then save + GC + reload — all sequenced by
    /// the DAG structure. No race conditions by construction.
    fn handle_commit_dag(
        &mut self,
        opstamp: crate::Opstamp,
        payload: Option<String>,
    ) -> crate::Result<crate::Opstamp> {
        // 1. Drain any active/explicit/pending merges (complete them inline)
        self.drain_all_merges();

        // 2. Collect ALL merge candidates (including cascading merges)
        let all_ops = self.collect_merge_candidates();

        // 3. Build and execute the commit DAG
        //    PrepareNode handles: purge_deletes, commit, start_merge
        //    MergeNodes handle: parallel merges
        //    FinalizeNode handles: end_merge, advance deletes
        //    Then: save_metas → gc → reload
        let mut dag = super::commit_dag::build_commit_dag(
            self.shared.clone(),
            all_ops,
            opstamp,
            payload,
        ).map_err(|e| crate::LucivyError::SystemError(format!("build DAG: {e}")))?;

        let dag_result = luciole::execute_dag(&mut dag, None)
            .map_err(|e| crate::LucivyError::SystemError(format!("execute DAG: {e}")))?;

        // Log the result
        eprintln!("{}", dag_result.display_summary());

        // Clear merge tracking (all merges done, GC already ran)
        self.segments_in_merge.clear();
        self.shared.gc_protected_segments.lock().unwrap().clear();

        Ok(opstamp)
    }

    fn handle_garbage_collect(&self) -> crate::Result<GarbageCollectionResult> {
        garbage_collect_files(&self.shared)
    }

    fn handle_start_merge(
        &mut self,
        merge_operation: MergeOperation,
        reply: ReplyPort,
        actor_state: &ActorState,
    ) {
        assert!(!merge_operation.segment_ids().is_empty());

        if self.explicit_merge.is_some() {
            reply.send_err(LucivyError::SystemError(
                "An explicit merge is already in progress".to_string(),
            ));
            return;
        }

        let segment_entries = match self
            .shared
            .segment_manager
            .start_merge(merge_operation.segment_ids())
        {
            Ok(entries) => entries,
            Err(err) => {
                warn!("Starting the merge failed (not fatal). {err}");
                reply.send_err(err);
                return;
            }
        };

        info!("Starting merge (explicit) - {:?}", merge_operation.segment_ids());
        self.track_segments(merge_operation.segment_ids());
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
                self.schedule_merge_step(actor_state);
            }
            Ok(None) => {
                self.shared.event_bus.emit(IndexEvent::MergeCompleted {
                    segment_ids: merge_operation.segment_ids().to_vec(),
                    duration: std::time::Duration::ZERO,
                    result_num_docs: 0,
                });
                self.untrack_segments(&merge_operation);
                match self.do_end_merge(merge_operation, None) {
                    Ok(()) => reply.send(SuOkReply),
                    Err(e) => reply.send_err(e),
                }
            }
            Err(e) => {
                self.shared.event_bus.emit(IndexEvent::MergeFailed {
                    segment_ids: merge_operation.segment_ids().to_vec(),
                    error: format!("{e:?}"),
                });
                self.untrack_segments(&merge_operation);
                reply.send_err(e);
            }
        }
    }

    fn do_end_merge(
        &mut self,
        merge_operation: MergeOperation,
        mut after_merge_segment_entry: Option<SegmentEntry>,
    ) -> crate::Result<()> {
        info!("End merge {:?}", after_merge_segment_entry.as_ref().map(|e| e.meta()));

        if let Some(entry) = after_merge_segment_entry.as_mut() {
            let mut delete_cursor = entry.delete_cursor().clone();
            if let Some(delete_operation) = delete_cursor.get() {
                let committed_opstamp = self.shared.load_meta().opstamp;
                if delete_operation.opstamp < committed_opstamp {
                    let index = &self.shared.index;
                    let segment = index.segment(entry.meta().clone());
                    if let Err(e) = crate::indexer::index_writer::advance_deletes(
                        segment, entry, committed_opstamp,
                    ) {
                        error!("Merge cancelled (advancing deletes failed): {:?}", e);
                        assert!(!cfg!(test), "Merge failed.");
                        return Err(e);
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
            self.shared.save_metas(previous_metas.opstamp, previous_metas.payload.clone())?;
        }

        let _ = garbage_collect_files(&self.shared);
        Ok(())
    }

    fn enqueue_merge_candidates(&mut self, actor_state: &ActorState) {
        let candidates = self.collect_merge_candidates();
        for op in candidates {
            self.pending_merges.push_back(op);
        }
        if self.active_merge.is_none() {
            self.start_next_incremental_merge(actor_state);
        }
    }

    fn start_next_incremental_merge(&mut self, actor_state: &ActorState) {
        while let Some(merge_op) = self.pending_merges.pop_front() {
            assert!(!merge_op.segment_ids().is_empty());

            let segment_entries = match self.shared.segment_manager.start_merge(merge_op.segment_ids()) {
                Ok(entries) => entries,
                Err(err) => {
                    warn!("Starting incremental merge failed (not fatal): {err}");
                    continue;
                }
            };

            info!("Starting merge (incremental) - {:?}", merge_op.segment_ids());
            self.track_segments(merge_op.segment_ids());
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
                    self.schedule_merge_step(actor_state);
                    return;
                }
                Ok(None) => {
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

    fn finish_explicit_merge(&mut self, explicit: ExplicitMerge, result: Option<SegmentEntry>) {
        let result_num_docs = result.as_ref().map(|e| e.meta().num_docs()).unwrap_or(0);
        let after_merge_meta = result.as_ref().map(|e| e.meta().clone());
        self.shared.event_bus.emit(IndexEvent::MergeCompleted {
            segment_ids: explicit.merge_operation.segment_ids().to_vec(),
            duration: explicit.start_time.elapsed(),
            result_num_docs,
        });
        self.untrack_segments(&explicit.merge_operation);
        match self.do_end_merge(explicit.merge_operation, result) {
            Ok(()) => explicit.reply.send(SuOkReply),
            Err(e) => explicit.reply.send_err(e),
        }
    }

    fn finish_incremental_merge(&mut self, active: ActiveMerge, result: Option<SegmentEntry>, actor_state: &ActorState) {
        let result_num_docs = result.as_ref().map(|e| e.meta().num_docs()).unwrap_or(0);
        self.shared.event_bus.emit(IndexEvent::MergeCompleted {
            segment_ids: active.merge_operation.segment_ids().to_vec(),
            duration: active.start_time.elapsed(),
            result_num_docs,
        });
        self.untrack_segments(&active.merge_operation);
        if let Err(e) = self.do_end_merge(active.merge_operation, result) {
            warn!("End merge (incremental) failed: {e:?}");
        }
        let new_candidates = self.collect_merge_candidates();
        for op in new_candidates {
            self.pending_merges.push_back(op);
        }
        self.start_next_incremental_merge(actor_state);
    }

    fn drain_all_merges(&mut self) {
        // Drain explicit merge
        if let Some(explicit) = self.explicit_merge.take() {
            let ExplicitMerge { merge_operation, mut state, start_time, reply } = explicit;
            loop {
                match state.step() {
                    StepResult::Continue => continue,
                    StepResult::Done(result) => {
                        let result_num_docs = result.as_ref().map(|e| e.meta().num_docs()).unwrap_or(0);
                        self.shared.event_bus.emit(IndexEvent::MergeCompleted {
                            segment_ids: merge_operation.segment_ids().to_vec(),
                            duration: start_time.elapsed(),
                            result_num_docs,
                        });
                        self.untrack_segments(&merge_operation);
                        match self.do_end_merge(merge_operation, result) {
                            Ok(()) => reply.send(SuOkReply),
                            Err(e) => reply.send_err(e),
                        }
                        break;
                    }
                }
            }
        }

        // Drain active merge
        if let Some(active) = self.active_merge.take() {
            let ActiveMerge { merge_operation, mut state, start_time } = active;
            loop {
                match state.step() {
                    StepResult::Continue => continue,
                    StepResult::Done(result) => {
                        let result_num_docs = result.as_ref().map(|e| e.meta().num_docs()).unwrap_or(0);
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

        // Drain pending merges
        while let Some(merge_op) = self.pending_merges.pop_front() {
            let segment_entries = match self.shared.segment_manager.start_merge(merge_op.segment_ids()) {
                Ok(entries) => entries,
                Err(err) => { warn!("Starting drain merge failed (not fatal): {err}"); continue; }
            };
            self.track_segments(merge_op.segment_ids());
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
                                let result_num_docs = result.as_ref().map(|e| e.meta().num_docs()).unwrap_or(0);
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

    fn emit_step_completed(&self, merge_op: &MergeOperation, state: &MergeState) {
        self.shared.event_bus.emit(IndexEvent::MergeStepCompleted {
            segment_ids: merge_op.segment_ids().to_vec(),
            steps_completed: state.steps_completed(),
            steps_total: state.estimated_steps(),
        });
    }

    fn untrack_segments(&mut self, merge_op: &MergeOperation) {
        for segment_id in merge_op.segment_ids() {
            self.segments_in_merge.remove(segment_id);
        }
        // Sync to shared GC protection
        if let Ok(mut protected) = self.shared.gc_protected_segments.lock() {
            *protected = self.segments_in_merge.clone();
        }
    }

    fn track_segments(&mut self, segment_ids: &[SegmentId]) {
        self.segments_in_merge.extend(segment_ids);
        if let Ok(mut protected) = self.shared.gc_protected_segments.lock() {
            *protected = self.segments_in_merge.clone();
        }
    }

    fn collect_merge_candidates(&self) -> Vec<MergeOperation> {
        let (mut committed_segments, mut uncommitted_segments) =
            self.shared.get_mergeable_segments(&self.segments_in_merge);

        let committed_docs: Vec<u32> = committed_segments.iter().map(|s| s.num_docs()).collect();
        let uncommitted_docs: Vec<u32> = uncommitted_segments.iter().map(|s| s.num_docs()).collect();
        lucivy_trace!("[merge_policy] segments: committed={:?} uncommitted={:?} in_merge={}",
            committed_docs, uncommitted_docs, self.segments_in_merge.len());

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
            .map(|mc| MergeOperation::new(current_opstamp, mc.0))
            .collect();

        let commit_opstamp = self.shared.load_meta().opstamp;
        let committed_merge_candidates = merge_policy
            .compute_merge_candidates(&committed_segments)
            .into_iter()
            .map(|mc| MergeOperation::new(commit_opstamp, mc.0));
        merge_candidates.extend(committed_merge_candidates);

        merge_candidates.retain(|op| op.segment_ids().len() > 1);

        for op in &merge_candidates {
            let seg_docs: Vec<String> = op.segment_ids().iter().map(|id| {
                committed_docs.iter().chain(uncommitted_docs.iter())
                    .find(|_| true) // just show count
                    .map(|_| format!("{}", id.uuid_string().chars().take(8).collect::<String>()))
                    .unwrap_or_default()
            }).collect();
            lucivy_trace!("[merge_policy] candidate: {} segments {:?}", op.segment_ids().len(), seg_docs);
        }

        merge_candidates
    }
}

// ─── Actor creation ─────────────────────────────────────────────────────────

pub(crate) fn create_segment_updater_actor(
    shared: Arc<SegmentUpdaterShared>,
) -> GenericActor {
    let mut actor = GenericActor::new("segment_updater");

    let su_state = SegmentUpdaterState {
        shared,
        active_merge: None,
        explicit_merge: None,
        pending_merges: VecDeque::new(),
        segments_in_merge: HashSet::new(),
    };
    actor.state_mut().insert::<SegmentUpdaterState>(su_state);

    // AddSegment: SegmentEntry in local
    actor.register(TypedHandler::<SuAddSegmentMsg, _>::new(
        |state, _msg, _reply, local| {
            let entry = local.and_then(|l| l.downcast::<SegmentEntry>().ok()).map(|e| *e);
            if let Some(entry) = entry {
                // Need to borrow actor_state for schedule_merge_step
                // Extract su_state, call handle, then put back — but we have &mut state
                let su = state.get_mut::<SegmentUpdaterState>().unwrap();
                su.shared.segment_manager.add_segment(entry);
                // enqueue_merge_candidates needs actor_state for self_ref
                // but we already have &mut state... extract self_ref first
                let self_ref_opt = state.get::<ActorRef<Envelope>>().cloned();
                let su = state.get_mut::<SegmentUpdaterState>().unwrap();
                let candidates = su.collect_merge_candidates();
                for op in candidates {
                    su.pending_merges.push_back(op);
                }
                if su.active_merge.is_none() {
                    // Start next merge — need self_ref for schedule_merge_step
                    // Inline start_next_incremental_merge without actor_state param
                    while let Some(merge_op) = su.pending_merges.pop_front() {
                        assert!(!merge_op.segment_ids().is_empty());
                        let segment_entries = match su.shared.segment_manager.start_merge(merge_op.segment_ids()) {
                            Ok(entries) => entries,
                            Err(err) => { warn!("Starting incremental merge failed: {err}"); continue; }
                        };
                        info!("Starting merge (incremental) - {:?}", merge_op.segment_ids());
                        su.track_segments(merge_op.segment_ids());
                        su.shared.event_bus.emit(IndexEvent::MergeStarted {
                            segment_ids: merge_op.segment_ids().to_vec(),
                            target_opstamp: merge_op.target_opstamp(),
                        });
                        let index = &su.shared.index;
                        let target_opstamp = merge_op.target_opstamp();
                        match MergeState::new(index, segment_entries, target_opstamp) {
                            Ok(Some(ms)) => {
                                su.active_merge = Some(ActiveMerge {
                                    merge_operation: merge_op,
                                    state: ms,
                                    start_time: Instant::now(),
                                });
                                if let Some(ref sr) = self_ref_opt {
                                    let _ = sr.send(SuMergeStepMsg.into_envelope());
                                }
                                break;
                            }
                            Ok(None) => {
                                su.untrack_segments(&merge_op);
                                if let Err(e) = su.do_end_merge(merge_op, None) {
                                    warn!("End merge (empty) failed: {e:?}");
                                }
                                continue;
                            }
                            Err(e) => {
                                warn!("Incremental merge state failed: {e:?}");
                                su.shared.event_bus.emit(IndexEvent::MergeFailed {
                                    segment_ids: merge_op.segment_ids().to_vec(),
                                    error: format!("{e:?}"),
                                });
                                su.untrack_segments(&merge_op);
                                if cfg!(test) { panic!("Merge state creation failed: {e:?}"); }
                                continue;
                            }
                        }
                    }
                }
            }
            ActorStatus::Continue
        },
    ));

    // Commit
    actor.register(TypedHandler::<SuCommitMsg, _>::new(
        |state, msg, reply, _local| {
            let su = state.get_mut::<SegmentUpdaterState>().unwrap();
            let result = su.handle_commit(msg.opstamp, msg.payload, msg.rebuild_sfx);
            // Enqueue merge candidates after commit — but NOT if we just
            // drained+rebuilt (rebuild_sfx=true), because that would create
            // new deferred segments after the rebuild.
            if !msg.rebuild_sfx {
                let self_ref_opt = state.get::<ActorRef<Envelope>>().cloned();
                let su = state.get_mut::<SegmentUpdaterState>().unwrap();
                let candidates = su.collect_merge_candidates();
                for op in candidates {
                    su.pending_merges.push_back(op);
                }
                if su.active_merge.is_none() && !su.pending_merges.is_empty() {
                    if let Some(ref sr) = self_ref_opt {
                        let _ = sr.send(SuMergeStepMsg.into_envelope());
                    }
                }
            }
            if let Some(reply) = reply {
                match result {
                    Ok(opstamp) => reply.send(SuOpsReply { opstamp }),
                    Err(e) => reply.send_err(e),
                }
            }
            ActorStatus::Continue
        },
    ));

    // GarbageCollect
    actor.register(TypedHandler::<SuGarbageCollectMsg, _>::new(
        |state, _msg, reply, _local| {
            let su = state.get_mut::<SegmentUpdaterState>().unwrap();
            let result = su.handle_garbage_collect();
            if let Some(reply) = reply {
                match result {
                    Ok(_gc) => reply.send(SuOkReply),
                    Err(e) => reply.send_err(e),
                }
            }
            ActorStatus::Continue
        },
    ));

    // StartMerge: MergeOperation in local
    actor.register(TypedHandler::<SuStartMergeMsg, _>::new(
        |state, _msg, reply, local| {
            let merge_op = local.and_then(|l| l.downcast::<MergeOperation>().ok()).map(|m| *m);
            if let (Some(merge_op), Some(reply)) = (merge_op, reply) {
                let self_ref_opt = state.get::<ActorRef<Envelope>>().cloned();
                let su = state.get_mut::<SegmentUpdaterState>().unwrap();
                // Inline handle_start_merge — needs actor_state for schedule_merge_step
                // Pass a dummy actor_state-like thing... actually just call and schedule after
                su.handle_start_merge(merge_op, reply, &ActorState::new());
                // Schedule merge step if needed
                if su.active_merge.is_some() || su.explicit_merge.is_some() {
                    if let Some(ref sr) = self_ref_opt {
                        let _ = sr.send(SuMergeStepMsg.into_envelope());
                    }
                }
            }
            ActorStatus::Continue
        },
    ));

    // MergeStep (self-message)
    actor.register(TypedHandler::<SuMergeStepMsg, _>::new(
        |state, _msg, _reply, _local| {
            let self_ref_opt = state.get::<ActorRef<Envelope>>().cloned();
            let su = state.get_mut::<SegmentUpdaterState>().unwrap();

            // If no merge is active but we have pending merges, start the next one.
            if su.active_merge.is_none() && su.explicit_merge.is_none() {
                while let Some(merge_op) = su.pending_merges.pop_front() {
                    assert!(!merge_op.segment_ids().is_empty());
                    let entries = match su.shared.segment_manager.start_merge(merge_op.segment_ids()) {
                        Ok(e) => e,
                        Err(err) => { warn!("Start pending merge failed: {err}"); continue; }
                    };
                    su.track_segments(merge_op.segment_ids());
                    su.shared.event_bus.emit(IndexEvent::MergeStarted {
                        segment_ids: merge_op.segment_ids().to_vec(),
                        target_opstamp: merge_op.target_opstamp(),
                    });
                    match MergeState::new(&su.shared.index, entries, merge_op.target_opstamp()) {
                        Ok(Some(ms)) => {
                            su.active_merge = Some(ActiveMerge {
                                merge_operation: merge_op,
                                state: ms,
                                start_time: Instant::now(),
                            });
                            break;
                        }
                        Ok(None) => {
                            su.untrack_segments(&merge_op);
                            let _ = su.do_end_merge(merge_op, None);
                            continue;
                        }
                        Err(e) => {
                            su.shared.event_bus.emit(IndexEvent::MergeFailed {
                                segment_ids: merge_op.segment_ids().to_vec(),
                                error: format!("{e:?}"),
                            });
                            su.untrack_segments(&merge_op);
                            if cfg!(test) { panic!("Merge state failed: {e:?}"); }
                            continue;
                        }
                    }
                }
            }

            // Handle explicit merge first
            if let Some(mut explicit) = su.explicit_merge.take() {
                match explicit.state.step() {
                    StepResult::Continue => {
                        su.emit_step_completed(&explicit.merge_operation, &explicit.state);
                        su.explicit_merge = Some(explicit);
                    }
                    StepResult::Done(result) => {
                        su.finish_explicit_merge(explicit, result);
                    }
                }
                if su.active_merge.is_some() || su.explicit_merge.is_some() {
                    if let Some(ref sr) = self_ref_opt {
                        let _ = sr.send(SuMergeStepMsg.into_envelope());
                    }
                }
                return ActorStatus::Continue;
            }

            // Then active merge
            if let Some(active) = su.active_merge.take() {
                let ActiveMerge { merge_operation, mut state, start_time } = active;
                match state.step() {
                    StepResult::Continue => {
                        su.emit_step_completed(&merge_operation, &state);
                        su.active_merge = Some(ActiveMerge { merge_operation, state, start_time });
                    }
                    StepResult::Done(result) => {
                        let result_num_docs = result.as_ref().map(|e| e.meta().num_docs()).unwrap_or(0);
                        su.shared.event_bus.emit(IndexEvent::MergeCompleted {
                            segment_ids: merge_operation.segment_ids().to_vec(),
                            duration: start_time.elapsed(),
                            result_num_docs,
                        });
                        su.untrack_segments(&merge_operation);
                        if let Err(e) = su.do_end_merge(merge_operation, result) {
                            warn!("End merge (incremental) failed: {e:?}");
                        }
                        // Re-collect candidates after merge
                        let new_candidates = su.collect_merge_candidates();
                        for op in new_candidates {
                            su.pending_merges.push_back(op);
                        }
                        // Try to start next
                        // (inline to avoid borrow issues)
                        while let Some(merge_op) = su.pending_merges.pop_front() {
                            assert!(!merge_op.segment_ids().is_empty());
                            let entries = match su.shared.segment_manager.start_merge(merge_op.segment_ids()) {
                                Ok(e) => e,
                                Err(err) => { warn!("Next merge failed: {err}"); continue; }
                            };
                            su.track_segments(merge_op.segment_ids());
                            su.shared.event_bus.emit(IndexEvent::MergeStarted {
                                segment_ids: merge_op.segment_ids().to_vec(),
                                target_opstamp: merge_op.target_opstamp(),
                            });
                            match MergeState::new(&su.shared.index, entries, merge_op.target_opstamp()) {
                                Ok(Some(new_state)) => {
                                    su.active_merge = Some(ActiveMerge {
                                        merge_operation: merge_op,
                                        state: new_state,
                                        start_time: Instant::now(),
                                    });
                                    break;
                                }
                                Ok(None) => {
                                    su.untrack_segments(&merge_op);
                                    let _ = su.do_end_merge(merge_op, None);
                                    continue;
                                }
                                Err(e) => {
                                    su.shared.event_bus.emit(IndexEvent::MergeFailed {
                                        segment_ids: merge_op.segment_ids().to_vec(),
                                        error: format!("{e:?}"),
                                    });
                                    su.untrack_segments(&merge_op);
                                    if cfg!(test) { panic!("Merge state failed: {e:?}"); }
                                    continue;
                                }
                            }
                        }
                    }
                }
                if su.active_merge.is_some() || su.explicit_merge.is_some() {
                    if let Some(ref sr) = self_ref_opt {
                        let _ = sr.send(SuMergeStepMsg.into_envelope());
                    }
                }
            }

            ActorStatus::Continue
        },
    ));

    // DrainMerges
    actor.register(TypedHandler::<SuDrainMergesMsg, _>::with_priority(
        |state, _msg, reply, _local| {
            let su = state.get_mut::<SegmentUpdaterState>().unwrap();
            // Full DAG commit: drain active merges, collect ALL candidates
            // (including cascading), execute them, save metas, GC.
            let opstamp = su.shared.load_meta().opstamp;
            let payload = su.shared.load_meta().payload.clone();
            match su.handle_commit_dag(opstamp, payload) {
                Ok(_) => {
                    if let Some(reply) = reply {
                        reply.send(SuOkReply);
                    }
                }
                Err(e) => {
                    error!("DAG drain failed: {e:?}");
                    if let Some(reply) = reply {
                        reply.send(SuOkReply); // still reply to unblock caller
                    }
                }
            }
            ActorStatus::Continue
        },
        Priority::Critical,
    ));

    // Kill
    actor.register(TypedHandler::<SuKillMsg, _>::new(
        |_state, _msg, _reply, _local| {
            ActorStatus::Stop
        },
    ));

    actor
}
