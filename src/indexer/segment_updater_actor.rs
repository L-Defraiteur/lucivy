//! SegmentUpdaterActor — segment management via GenericActor.
//!
//! All commits and merges go through a single DAG pipeline.
//! No background merge state machine, no drain, no double save_metas.

use std::sync::Arc;

use crate::actor::actor_state::ActorState;
use crate::actor::envelope::{type_tag_hash, Envelope, Message, ReplyPort};
use crate::actor::generic_actor::GenericActor;
use crate::actor::handler::TypedHandler;
use crate::actor::mailbox::ActorRef;
use crate::actor::{ActorStatus, Priority};
use crate::directory::GarbageCollectionResult;
use crate::index::SegmentId;
use crate::indexer::events::IndexEvent;
use crate::indexer::merge_operation::MergeOperation;
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
}

impl SegmentUpdaterState {
    /// Execute a commit via DAG. Loops until no more merge candidates (cascade).
    fn handle_commit(
        &mut self,
        opstamp: crate::Opstamp,
        payload: Option<String>,
    ) -> crate::Result<crate::Opstamp> {
        let start = std::time::Instant::now();
        self.shared.event_bus.emit(IndexEvent::CommitStarted { opstamp });

        loop {
            let merge_candidates = self.collect_merge_candidates();
            let no_merges = merge_candidates.is_empty();

            let mut dag = super::commit_dag::build_commit_dag(
                self.shared.clone(),
                merge_candidates,
                opstamp,
                payload.clone(),
            ).map_err(|e| crate::LucivyError::SystemError(format!("build DAG: {e}")))?;

            let dag_result = luciole::execute_dag(&mut dag, None)
                .map_err(|e| crate::LucivyError::SystemError(format!("execute DAG: {e}")))?;

            eprintln!("{}", dag_result.display_summary());

            if no_merges {
                break;
            }
        }

        self.shared.event_bus.emit(IndexEvent::CommitCompleted {
            opstamp,
            duration: start.elapsed(),
        });

        Ok(opstamp)
    }

    /// Execute an explicit merge via DAG.
    fn handle_merge(
        &mut self,
        merge_operation: MergeOperation,
    ) -> crate::Result<()> {
        let meta = self.shared.load_meta();

        let mut dag = super::commit_dag::build_commit_dag(
            self.shared.clone(),
            vec![merge_operation],
            meta.opstamp,
            meta.payload.clone(),
        ).map_err(|e| crate::LucivyError::SystemError(format!("build merge DAG: {e}")))?;

        let dag_result = luciole::execute_dag(&mut dag, None)
            .map_err(|e| crate::LucivyError::SystemError(format!("execute merge DAG: {e}")))?;

        eprintln!("{}", dag_result.display_summary());

        Ok(())
    }

    fn handle_garbage_collect(&self) -> crate::Result<GarbageCollectionResult> {
        garbage_collect_files(&self.shared)
    }

    fn collect_merge_candidates(&self) -> Vec<MergeOperation> {
        // At commit time, all segments end up in committed (PrepareNode does
        // segment_manager.commit()). Treat them as one pool so the merge
        // policy sees the full picture — no more split committed/uncommitted.
        let (committed, uncommitted) =
            self.shared.get_mergeable_segments(&std::collections::HashSet::new());

        let mut all_segments: Vec<crate::index::SegmentMeta> = committed;
        all_segments.extend(uncommitted);

        let docs: Vec<u32> = all_segments.iter().map(|s| s.num_docs()).collect();
        lucivy_trace!("[merge_policy] segments: {:?}", docs);

        // Single segment with no deletes → nothing to merge
        if all_segments.len() <= 1
            && all_segments.first().map_or(true, |s| s.num_deleted_docs() == 0)
        {
            return vec![];
        }

        let merge_policy = self.shared.get_merge_policy();
        let opstamp = self.shared.load_meta().opstamp;

        merge_policy
            .compute_merge_candidates(&all_segments)
            .into_iter()
            .map(|mc| MergeOperation::new(opstamp, mc.0))
            .filter(|op| op.segment_ids().len() > 1)
            .collect()
    }
}

// ─── Actor creation ─────────────────────────────────────────────────────────

pub(crate) fn create_segment_updater_actor(
    shared: Arc<SegmentUpdaterShared>,
) -> GenericActor {
    let mut actor = GenericActor::new("segment_updater");

    let su_state = SegmentUpdaterState { shared };
    actor.state_mut().insert::<SegmentUpdaterState>(su_state);

    // AddSegment: SegmentEntry in local
    actor.register(TypedHandler::<SuAddSegmentMsg, _>::new(
        |state, _msg, _reply, local| {
            let entry = local.and_then(|l| l.downcast::<SegmentEntry>().ok()).map(|e| *e);
            if let Some(entry) = entry {
                let su = state.get_mut::<SegmentUpdaterState>().unwrap();
                su.shared.segment_manager.add_segment(entry);
            }
            ActorStatus::Continue
        },
    ));

    // Commit — everything goes through the DAG
    actor.register(TypedHandler::<SuCommitMsg, _>::new(
        |state, msg, reply, _local| {
            let su = state.get_mut::<SegmentUpdaterState>().unwrap();
            let result = su.handle_commit(msg.opstamp, msg.payload);
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

    // StartMerge: MergeOperation in local — executes merge DAG inline
    actor.register(TypedHandler::<SuStartMergeMsg, _>::new(
        |state, _msg, reply, local| {
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

    // Kill
    actor.register(TypedHandler::<SuKillMsg, _>::new(
        |_state, _msg, _reply, _local| {
            ActorStatus::Stop
        },
    ));

    actor
}
