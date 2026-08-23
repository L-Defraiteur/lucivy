//! SegmentUpdaterActor — segment management via GenericActor.
//!
//! All commits and merges go through a single DAG pipeline.
//! No background merge state machine, no drain, no double save_metas.

use std::sync::Arc;

use crate::actor::envelope::{type_tag_hash, Message};
use crate::actor::generic_actor::GenericActor;
use crate::actor::handler::TypedHandler;
use crate::actor::ActorStatus;
use crate::directory::GarbageCollectionResult;
use crate::indexer::events::IndexEvent;
use crate::indexer::merge_operation::MergeOperation;
use crate::indexer::segment_updater::{garbage_collect_files, SegmentUpdaterShared};
use crate::indexer::SegmentEntry;

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

/// Start several merges at once. The operations travel in `local` as a
/// `Vec<MergeOperation>`; the heavy work runs as scheduler tasks, and the actor
/// is notified through `SuMergesDoneMsg` when all of them have finished.
pub(crate) struct SuStartMergesMsg;
impl Message for SuStartMergesMsg {
    fn type_tag() -> u64 { type_tag_hash(b"SuStartMergesMsg") }
    fn encode(&self) -> Vec<u8> { vec![] }
    fn decode(_: &[u8]) -> Result<Self, String> { Ok(Self) }
}

/// Internal: the merge tasks of one `SuStartMergesMsg` have all completed.
/// `local` carries `MergesDone`.
pub(crate) struct SuMergesDoneMsg;
impl Message for SuMergesDoneMsg {
    fn type_tag() -> u64 { type_tag_hash(b"SuMergesDoneMsg") }
    fn encode(&self) -> Vec<u8> { vec![] }
    fn decode(_: &[u8]) -> Result<Self, String> { Ok(Self) }
}

/// What a finished batch of merge tasks hands back to the actor.
pub(crate) struct MergesDone {
    /// One slot per operation, filled by its task. `None` = the task failed.
    pub results: Arc<std::sync::Mutex<Vec<Option<super::commit_dag::MergeResult>>>>,
    /// First task error, if any.
    pub errors: Vec<String>,
    pub reply: Option<crate::actor::envelope::ReplyPort>,
    /// Every segment the batch took, to release from `merging` whether its
    /// task succeeded or not.
    pub segment_ids: Vec<crate::index::SegmentId>,
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

pub(crate) struct SegmentUpdaterState {
    shared: Arc<SegmentUpdaterShared>,
    /// Segments handed to an in-flight merge task. The merge policy must not
    /// see them again before `handle_merges_done` registers the result.
    merging: std::collections::HashSet<crate::index::SegmentId>,
}

impl SegmentUpdaterState {
    /// Execute a commit via DAG, then schedule merges asynchronously.
    ///
    /// The commit (prepare + save_metas) runs inline — it's fast.
    /// Merges are deferred to a background task via submit_task so the
    /// scheduler thread is freed immediately after commit.
    fn handle_commit(
        &mut self,
        opstamp: crate::Opstamp,
        payload: Option<String>,
        self_ref: Option<crate::actor::mailbox::ActorRef<crate::actor::envelope::Envelope>>,
    ) -> crate::Result<crate::Opstamp> {
        let start = std::time::Instant::now();
        self.shared.event_bus.emit(IndexEvent::CommitStarted { opstamp });

        // Phase 1: commit without merges (fast — just save_metas).
        let mut dag = super::commit_dag::build_commit_dag(
            self.shared.clone(),
            vec![], // no merges
            opstamp,
            payload.clone(),
        ).map_err(|e| crate::LucivyError::SystemError(format!("build DAG: {e}")))?;

        let dag_result = luciole::execute_dag(&mut dag, None)
            .map_err(|e| crate::LucivyError::SystemError(format!("execute DAG: {e}")))?;

        if crate::diag::is_verbose() {
            eprintln!("{}", dag_result.display_summary());
        }

        self.shared.event_bus.emit(IndexEvent::CommitCompleted {
            opstamp,
            duration: start.elapsed(),
        });

        if let Some(self_ref) = self_ref {
            self.policy_merges(opstamp, None, self_ref)?;
        }

        Ok(opstamp)
    }

    /// Ask the merge policy and start what it proposes. Until 23 August 2026
    /// nothing consulted it: every merge came from an explicit start_merge(),
    /// and a writer left alone produced one segment per commit forever.
    /// Merges no longer block the actor (they are scheduler tasks, see
    /// handle_start_merges), so there is no reason left to defer them.
    /// Segments already in a running merge are skipped; the policy's caps
    /// keep segments from growing past what the v3 encoding can address.
    /// Called after each commit and after each finished batch (cascade).
    /// Returns true when a batch was started.
    fn policy_merges(
        &mut self,
        opstamp: crate::Opstamp,
        payload: Option<String>,
        self_ref: crate::actor::mailbox::ActorRef<crate::actor::envelope::Envelope>,
    ) -> crate::Result<bool> {
        let policy = self.shared.merge_policy.read().unwrap().clone();
        let metas: Vec<crate::index::SegmentMeta> = self.shared.segment_manager
            .committed_segment_metas()
            .into_iter()
            .filter(|m| !self.merging.contains(&m.id()))
            .collect();
        let ops: Vec<MergeOperation> = policy
            .compute_merge_candidates(&metas)
            .into_iter()
            .filter(|c| c.0.len() > 1)
            .map(|c| MergeOperation::new(opstamp, c.0))
            .collect();
        if ops.is_empty() {
            return Ok(false);
        }
        if crate::diag::is_verbose() {
            eprintln!("[segment_updater] policy: {} merge(s) at opstamp {opstamp}: {:?}",
                ops.len(), ops.iter().map(|o| o.segment_ids().len()).collect::<Vec<_>>());
        }
        let _ = payload;
        self.handle_start_merges(ops, None, self_ref)?;
        Ok(true)
    }

    /// Prepare several merges inline, then run each one as a scheduler task.
    ///
    /// The commit DAG has always had one `merge_i` node per operation, fanned
    /// out from `prepare` — but `execute_dag` runs every level inline when it
    /// is called from an actor, which is exactly where this DAG runs. So the
    /// fan-out was parallel on paper and sequential in practice: twenty merges
    /// of ~700ms cost fourteen seconds, on a machine with twenty-four cores.
    ///
    /// What must stay inside the actor is the bookkeeping: purging, committing
    /// the segment manager, marking segments as merging, and later registering
    /// the results. What does not is the merge itself — reading source segments
    /// and writing a new one — which touches nothing but `shared.index`. That
    /// part goes to the scheduler as one task per operation, and the actor is
    /// told through `SuMergesDoneMsg` when all are done. It never waits.
    fn handle_start_merges(
        &mut self,
        merge_ops: Vec<MergeOperation>,
        reply: Option<crate::actor::envelope::ReplyPort>,
        self_ref: crate::actor::mailbox::ActorRef<crate::actor::envelope::Envelope>,
    ) -> crate::Result<()> {
        let meta = self.shared.load_meta();
        let opstamp = meta.opstamp;

        // Prepare, exactly as PrepareNode does.
        let segment_entries = self.shared.purge_deletes(opstamp)?;
        self.shared.segment_manager.commit(segment_entries);

        let batch_ids: Vec<crate::index::SegmentId> = merge_ops.iter()
            .flat_map(|op| op.segment_ids().iter().copied()).collect();
        self.refuse_if_merging(&batch_ids)?;
        let mut prepared: Vec<(MergeOperation, Vec<SegmentEntry>)> = Vec::new();
        for op in &merge_ops {
            let entries = self.shared.segment_manager.start_merge(op.segment_ids())?;
            self.merging.extend(op.segment_ids().iter().copied());
            self.shared.event_bus.emit(IndexEvent::MergeStarted {
                segment_ids: op.segment_ids().to_vec(),
                target_opstamp: op.target_opstamp(),
            });
            prepared.push((MergeOperation::new(op.target_opstamp(), op.segment_ids().to_vec()), entries));
        }

        let results: Arc<std::sync::Mutex<Vec<Option<super::commit_dag::MergeResult>>>> =
            Arc::new(std::sync::Mutex::new((0..prepared.len()).map(|_| None).collect()));

        let scheduler = crate::actor::scheduler::global_scheduler();
        let mut rxs = Vec::with_capacity(prepared.len());
        for (i, (op, entries)) in prepared.into_iter().enumerate() {
            let shared = self.shared.clone();
            let slot = results.clone();
            rxs.push(scheduler.submit_task(crate::actor::Priority::High, move || {
                match super::commit_dag::run_merge(&shared, op, entries) {
                    Ok(r) => {
                        slot.lock().unwrap()[i] = Some(r);
                        Ok(())
                    }
                    Err(e) => Err(e),
                }
            }));
        }

        self.shared.pending_merge_tasks.store(true, std::sync::atomic::Ordering::Release);

        let payload = meta.payload.clone();
        crate::actor::reply::collect_replies_to(
            rxs,
            &self_ref,
            "start_merges",
            move |task_results: Vec<Result<(), String>>| {
                let errors: Vec<String> = task_results.into_iter()
                    .filter_map(|r| r.err()).collect();
                let local: Box<dyn std::any::Any + Send> =
                    Box::new((MergesDone { results, errors, reply, segment_ids: batch_ids.clone() }, opstamp, payload));
                crate::actor::envelope::Envelope {
                    type_tag: SuMergesDoneMsg::type_tag(),
                    payload: SuMergesDoneMsg.encode(),
                    reply: None,
                    local: Some(local),
                }
            },
        );
        Ok(())
    }

    /// Register finished merges: end_merge per result, save metas, GC. This is
    /// FinalizeNode + SaveMetasNode + GCNode, run inline — all bookkeeping.
    fn handle_merges_done(
        &mut self,
        done: MergesDone,
        opstamp: crate::Opstamp,
        payload: Option<String>,
        self_ref: Option<crate::actor::mailbox::ActorRef<crate::actor::envelope::Envelope>>,
    ) -> crate::Result<()> {
        for id in &done.segment_ids { self.merging.remove(id); }
        // `pending_merge_tasks` stays up until the cascade below decides
        // nothing else starts: drain_merges() polls it, and a reader that
        // persists or queries between two batches sees half-written files.
        let mut still_pending = false;
        let r = self.merges_done_inner(done, opstamp, payload.clone());
        if r.is_ok() {
            if let Some(self_ref) = self_ref {
                still_pending = self.policy_merges(opstamp, payload, self_ref)?;
            }
        }
        if !still_pending {
            self.shared.pending_merge_tasks.store(false, std::sync::atomic::Ordering::Release);
        }
        r
    }

    fn merges_done_inner(
        &mut self,
        done: MergesDone,
        opstamp: crate::Opstamp,
        payload: Option<String>,
    ) -> crate::Result<()> {

        let results: Vec<Option<super::commit_dag::MergeResult>> =
            std::mem::take(&mut *done.results.lock().unwrap());

        let mut first_err: Option<String> = done.errors.first().cloned();
        for result in results.into_iter().flatten() {
            if let Err(e) = super::commit_dag::register_merge_result(&self.shared, result) {
                first_err.get_or_insert(e);
            }
        }
        if let Some(e) = first_err {
            return Err(crate::LucivyError::SystemError(format!("merge: {e}")));
        }

        self.shared.save_metas(opstamp, payload)?;
        let _ = garbage_collect_files(&self.shared)?;
        Ok(())
    }

    /// Execute an explicit merge via DAG.
    /// Two merges over one segment corrupt the index: both read it, both
    /// register a replacement, and the second registration drops documents
    /// (measured: 400 → 269 on `v3_merge_preserves_results` the day the
    /// policy was wired to commit). Explicit merges that overlap a running
    /// one are refused, not queued — the caller can retry after
    /// `wait_merging_threads`, or set `NoMergePolicy` to drive merges itself.
    fn refuse_if_merging(&self, ids: &[crate::index::SegmentId]) -> crate::Result<()> {
        if let Some(id) = ids.iter().find(|id| self.merging.contains(id)) {
            return Err(crate::LucivyError::InvalidArgument(format!(
                "segment {} is being merged by a running merge", id.short_uuid_string())));
        }
        Ok(())
    }

    fn handle_merge(
        &mut self,
        merge_operation: MergeOperation,
    ) -> crate::Result<()> {
        self.refuse_if_merging(merge_operation.segment_ids())?;
        let meta = self.shared.load_meta();

        let mut dag = super::commit_dag::build_commit_dag(
            self.shared.clone(),
            vec![merge_operation],
            meta.opstamp,
            meta.payload.clone(),
        ).map_err(|e| crate::LucivyError::SystemError(format!("build merge DAG: {e}")))?;

        let dag_result = luciole::execute_dag(&mut dag, None)
            .map_err(|e| crate::LucivyError::SystemError(format!("execute merge DAG: {e}")))?;

        if crate::diag::is_verbose() {
            eprintln!("{}", dag_result.display_summary());
        }

        Ok(())
    }

    fn handle_garbage_collect(&self) -> crate::Result<GarbageCollectionResult> {
        garbage_collect_files(&self.shared)
    }

}

// ─── Actor creation ─────────────────────────────────────────────────────────

pub(crate) fn create_segment_updater_actor(
    shared: Arc<SegmentUpdaterShared>,
) -> GenericActor {
    let mut actor = GenericActor::new("segment_updater");

    let su_state = SegmentUpdaterState { shared, merging: Default::default() };
    actor.state_mut().insert::<SegmentUpdaterState>(su_state);

    // AddSegment: SegmentEntry in local
    actor.register(TypedHandler::<SuAddSegmentMsg, _>::new(
        |state, _msg, _reply, local, _ctx| {
            let entry = local.and_then(|l| l.downcast::<SegmentEntry>().ok()).map(|e| *e);
            if let Some(entry) = entry {
                let su = state.get_mut::<SegmentUpdaterState>().unwrap();
                su.shared.segment_manager.add_segment(entry);
            }
            ActorStatus::Continue
        },
    ));

    // Commit — inline (cooperative wait inside handler is OK here:
    // downstream actors use Suspend, so no deadlock risk. Will be
    // migrated to submit_task once actor lifecycle management is in place).
    actor.register(TypedHandler::<SuCommitMsg, _>::new(
        |state, msg, reply, _local, _ctx| {
            let self_ref = state
                .get::<crate::actor::mailbox::ActorRef<crate::actor::envelope::Envelope>>()
                .cloned();
            let su = state.get_mut::<SegmentUpdaterState>().unwrap();
            let result = su.handle_commit(msg.opstamp, msg.payload, self_ref);
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
        |state, _msg, reply, _local, _ctx| {
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

    // StartMerge: MergeOperation in local — executes merge DAG inline
    actor.register(TypedHandler::<SuStartMergeMsg, _>::new(
        |state, _msg, reply, local, _ctx| {
            let merge_op = local.and_then(|l| l.downcast::<MergeOperation>().ok()).map(|m| *m);
            if let (Some(merge_op), Some(reply)) = (merge_op, reply) {
                let su = state.get_mut::<SegmentUpdaterState>().unwrap();
                match su.handle_merge(merge_op) {
                    Ok(()) => reply.send(SuOkReply),
                    Err(e) => reply.send_err(e),
                }
            }
            ActorStatus::Continue
        },
    ));

    // StartMerges: Vec<MergeOperation> in local — prepares inline, runs the
    // merges as tasks, replies from the SuMergesDoneMsg handler.
    actor.register(TypedHandler::<SuStartMergesMsg, _>::new(
        |state, _msg, reply, local, _ctx| {
            let ops = local.and_then(|l| l.downcast::<Vec<MergeOperation>>().ok()).map(|m| *m);
            let Some(ops) = ops else { return ActorStatus::Continue };
            let self_ref = state
                .get::<crate::actor::mailbox::ActorRef<crate::actor::envelope::Envelope>>()
                .unwrap().clone();
            let su = state.get_mut::<SegmentUpdaterState>().unwrap();
            if let Err(e) = su.handle_start_merges(ops, reply, self_ref) {
                // `reply` moved into the call; on a prepare failure it was
                // consumed there and nothing answers. Keep the failure loud.
                eprintln!("[segment_updater] start_merges failed: {e}");
            }
            ActorStatus::Continue
        },
    ));

    actor.register(TypedHandler::<SuMergesDoneMsg, _>::new(
        |state, _msg, _reply, local, _ctx| {
            let Some(local) = local else { return ActorStatus::Continue };
            let Ok(boxed) = local.downcast::<(MergesDone, crate::Opstamp, Option<String>)>()
            else { return ActorStatus::Continue };
            let (mut done, opstamp, payload) = *boxed;
            let reply = done.reply.take();
            let self_ref = state
                .get::<crate::actor::mailbox::ActorRef<crate::actor::envelope::Envelope>>()
                .cloned();
            let su = state.get_mut::<SegmentUpdaterState>().unwrap();
            match su.handle_merges_done(done, opstamp, payload, self_ref) {
                Ok(()) => { if let Some(r) = reply { r.send(SuOkReply); } }
                Err(e) => { if let Some(r) = reply { r.send_err(e); } }
            }
            ActorStatus::Continue
        },
    ));

    // Kill
    actor.register(TypedHandler::<SuKillMsg, _>::new(
        |_state, _msg, _reply, _local, _ctx| {
            ActorStatus::Stop
        },
    ));

    actor
}
