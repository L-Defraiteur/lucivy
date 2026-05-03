//! IndexerActor — document indexation via GenericActor.
//!
//! Each IndexerActor receives batches of documents via its mailbox,
//! accumulates them in a segment, and finalizes on Flush or when
//! the memory budget is reached.
//!
//! **Background finalize**: when the memory budget is reached, the segment
//! finalize (SfxCollector.build + remap_and_write) runs in a background
//! FinalizerActor while the IndexerActor immediately starts a new segment.
//! This pipelines the expensive finalize with the next batch of documents.

use crate::actor::envelope::{type_tag_hash, Envelope, Message};
use crate::actor::generic_actor::GenericActor;
use crate::actor::handler::TypedHandler;
use crate::actor::mailbox::{mailbox, ActorRef};
use crate::actor::reply::ReplyReceiver;
use crate::actor::scheduler::global_scheduler;
use crate::actor::{ActorStatus, Priority};
use crate::index::{Index, Segment};
use crate::indexer::delete_queue::DeleteCursor;
use crate::indexer::index_writer::{finalize_segment, MARGIN_IN_BYTES};
use crate::indexer::index_writer_status::IndexWriterBomb;
use crate::indexer::segment_updater::SegmentUpdater;
use crate::indexer::{AddBatch, SegmentWriter};
use crate::schema::document::Document;

// ─── Messages ───────────────────────────────────────────────────────────────

/// Batch of documents to index. The AddBatch<D> is in Envelope.local.
pub(crate) struct IndexerDocsMsg;

impl Message for IndexerDocsMsg {
    fn type_tag() -> u64 { type_tag_hash(b"IndexerDocsMsg") }
    fn encode(&self) -> Vec<u8> { vec![] }
    fn decode(_: &[u8]) -> Result<Self, String> { Ok(Self) }
}

/// Flush the current segment and reply when done.
pub(crate) struct IndexerFlushMsg;

impl Message for IndexerFlushMsg {
    fn type_tag() -> u64 { type_tag_hash(b"IndexerFlushMsg") }
    fn encode(&self) -> Vec<u8> { vec![] }
    fn decode(_: &[u8]) -> Result<Self, String> { Ok(Self) }
}

/// Reply: Ok(()) or error string.
pub(crate) struct IndexerFlushReply;

impl Message for IndexerFlushReply {
    fn type_tag() -> u64 { type_tag_hash(b"IndexerFlushReply") }
    fn encode(&self) -> Vec<u8> { vec![] }
    fn decode(_: &[u8]) -> Result<Self, String> { Ok(Self) }
}

/// Internal: finalize task completed during flush — now send the flush reply.
/// The Result<Vec<u8>, Vec<u8>> from the task is in Envelope.local.
struct IndexerFinalizeCompleteMsg;

impl Message for IndexerFinalizeCompleteMsg {
    fn type_tag() -> u64 { type_tag_hash(b"IndexerFinalizeCompleteMsg") }
    fn encode(&self) -> Vec<u8> { vec![] }
    fn decode(_: &[u8]) -> Result<Self, String> { Ok(Self) }
}

/// Drain: reply immediately, proving all prior messages were processed (FIFO).
pub(crate) struct IndexerDrainMsg;

impl Message for IndexerDrainMsg {
    fn type_tag() -> u64 { type_tag_hash(b"IndexerDrainMsg") }
    fn encode(&self) -> Vec<u8> { vec![] }
    fn decode(_: &[u8]) -> Result<Self, String> { Ok(Self) }
}

/// Drain reply.
pub(crate) struct IndexerDrainReply;

impl Message for IndexerDrainReply {
    fn type_tag() -> u64 { type_tag_hash(b"IndexerDrainReply") }
    fn encode(&self) -> Vec<u8> { vec![] }
    fn decode(_: &[u8]) -> Result<Self, String> { Ok(Self) }
}

/// Shutdown the actor cleanly.
pub(crate) struct IndexerShutdownMsg;

impl Message for IndexerShutdownMsg {
    fn type_tag() -> u64 { type_tag_hash(b"IndexerShutdownMsg") }
    fn encode(&self) -> Vec<u8> { vec![] }
    fn decode(_: &[u8]) -> Result<Self, String> { Ok(Self) }
}

// ─── Finalizer messages ─────────────────────────────────────────────────────

/// Finalize a segment in the background.
/// Envelope.local: FinalizeWork (Segment, SegmentWriter, DeleteCursor, SegmentUpdater).
struct FinalizeMsg;

impl Message for FinalizeMsg {
    fn type_tag() -> u64 { type_tag_hash(b"FinalizeMsg") }
    fn encode(&self) -> Vec<u8> { vec![] }
    fn decode(_: &[u8]) -> Result<Self, String> { Ok(Self) }
}

/// Finalize reply (empty on success, error bytes on failure).
struct FinalizeReply;

impl Message for FinalizeReply {
    fn type_tag() -> u64 { type_tag_hash(b"FinalizeReply") }
    fn encode(&self) -> Vec<u8> { vec![] }
    fn decode(_: &[u8]) -> Result<Self, String> { Ok(Self) }
}

/// Work payload for the FinalizerActor (passed via Envelope.local).
struct FinalizeWork {
    segment: Segment,
    writer: SegmentWriter,
    delete_cursor: DeleteCursor,
    segment_updater: SegmentUpdater,
}

// ─── FinalizerActor ─────────────────────────────────────────────────────────

/// Create a FinalizerActor — runs finalize_segment() in background.
///
/// One FinalizerActor per IndexerActor. Receives FinalizeMsg with work payload,
/// runs the expensive finalize (SfxCollector.build + remap_and_write), and replies.
fn create_finalizer_actor() -> GenericActor {
    let mut actor = GenericActor::new("finalizer");

    actor.register(TypedHandler::<FinalizeMsg, _>::new(
        |_state, _msg, reply, local, _ctx| {
            let mut work = *local.unwrap().downcast::<FinalizeWork>().unwrap();

            let result = if work.segment_updater.is_alive() {
                finalize_segment(
                    work.segment,
                    work.writer,
                    &work.segment_updater,
                    &mut work.delete_cursor,
                )
            } else {
                Ok(())
            };

            if let Some(reply) = reply {
                match result {
                    Ok(()) => reply.send(FinalizeReply),
                    Err(e) => reply.send_err(e),
                }
            }

            ActorStatus::Continue
        },
    ));

    actor
}

/// Spawn a FinalizerActor in the global scheduler and return its ActorRef.
fn spawn_finalizer_actor() -> ActorRef<Envelope> {
    let scheduler = global_scheduler();
    let actor = create_finalizer_actor();
    let (mb, mut actor_ref) = mailbox::<Envelope>(4);
    scheduler.spawn(actor, mb, &mut actor_ref, 4);
    actor_ref
}

// ─── State ──────────────────────────────────────────────────────────────────

/// Segment currently being written.
struct SegmentInProgress {
    segment: Segment,
    writer: SegmentWriter,
}

/// Maximum documents processed before the indexer yields the scheduler thread.
/// Prevents starvation: drain, flush, finalize tasks can run between chunks.
const YIELD_EVERY_N_DOCS: usize = 64;

/// All state for the indexer, wrapped in a single struct to avoid
/// TypeId collisions in ActorState.
struct IndexerState<D: Document> {
    segment_updater: SegmentUpdater,
    index: Index,
    mem_budget: usize,
    delete_cursor: DeleteCursor,
    bomb: Option<IndexWriterBomb<D>>,
    current: Option<SegmentInProgress>,
    pending_error: Option<crate::LucivyError>,
    /// ActorRef to the background FinalizerActor.
    finalizer_ref: ActorRef<Envelope>,
    /// Pending background finalize (at most one).
    /// Ok(bytes) = success, Err(bytes) = serialized LucivyError.
    pending_finalize: Option<ReplyReceiver<Result<Vec<u8>, Vec<u8>>>>,
    /// Documents processed since last yield. Reset on Yield.
    docs_since_yield: usize,
}

impl<D: Document> IndexerState<D> {
    /// Process a batch of documents. When memory budget is reached,
    /// submit the finalize as a task (runs on a pool thread, NOT in a
    /// handler). The handler returns immediately — thread freed.
    ///
    /// Returns true if a finalize was dispatched (caller should Yield).
    fn handle_docs(&mut self, batch: AddBatch<D>) -> bool {
        if batch.is_empty() || self.pending_error.is_some() {
            return false;
        }

        // Poll previous background finalize (non-blocking).
        self.poll_pending_finalize();

        let current = match &mut self.current {
            Some(c) => c,
            None => {
                self.delete_cursor.skip_to(batch[0].opstamp);
                let segment = self.index.new_segment();
                let writer = match SegmentWriter::for_segment(self.mem_budget, segment.clone()) {
                    Ok(w) => w,
                    Err(e) => {
                        self.set_error(e);
                        return false;
                    }
                };
                self.current = Some(SegmentInProgress { segment, writer });
                self.current.as_mut().unwrap()
            }
        };

        for doc in batch {
            if let Err(e) = current.writer.add_document(doc) {
                self.current.take();
                self.set_error(e);
                return false;
            }
        }

        if current.writer.mem_usage() >= self.mem_budget - MARGIN_IN_BYTES {
            info!(
                "Buffer limit reached, flushing segment with maxdoc={}.",
                current.writer.max_doc()
            );
            self.submit_finalize_task();
            self.docs_since_yield = 0;
            return true; // Yield to free the scheduler thread.
        }

        // Yield periodically to prevent starvation. Without this, the indexer
        // can hold a scheduler thread for thousands of add_document calls,
        // blocking drain/flush/finalize tasks from running.
        self.docs_since_yield += 1;
        if self.docs_since_yield >= YIELD_EVERY_N_DOCS {
            self.docs_since_yield = 0;
            return true; // Yield — scheduler can dispatch other work.
        }
        false
    }

    /// Flush: submit current segment for background finalize.
    ///
    /// Returns the list of pending finalize receivers (0, 1, or 2).
    /// The caller is responsible for waiting on these — NOT this function.
    /// No blocking wait in the handler.
    fn handle_flush(&mut self) -> crate::Result<Vec<ReplyReceiver<Result<Vec<u8>, Vec<u8>>>>> {
        if let Some(err) = self.pending_error.take() {
            return Err(err);
        }
        let mut rxs = Vec::new();
        // Collect any pending finalize from handle_docs.
        if let Some(rx) = self.pending_finalize.take() {
            rxs.push(rx);
        }
        // Submit current segment to background (if any).
        self.submit_finalize_task();
        if let Some(rx) = self.pending_finalize.take() {
            rxs.push(rx);
        }
        Ok(rxs)
    }

    fn handle_shutdown(&mut self) {
        self.submit_finalize_task();
        let _ = self.wait_pending_finalize();
        if let Some(bomb) = self.bomb.take() {
            bomb.defuse();
        }
    }

    fn set_error(&mut self, err: crate::LucivyError) {
        self.pending_error = Some(err);
        drop(self.bomb.take());
    }

    /// Submit the current segment for finalization on a pool thread.
    ///
    /// The heavy work (SfxCollector.build, remap_and_write, segment_updater
    /// schedule_add_segment) runs on a task thread — NOT in an actor handler.
    /// The handler returns immediately.
    fn submit_finalize_task(&mut self) {
        let current = match self.current.take() {
            Some(c) => c,
            None => return,
        };
        if !self.segment_updater.is_alive() {
            return;
        }

        let segment = current.segment;
        let writer = current.writer;
        let mut delete_cursor = self.delete_cursor.clone();
        let segment_updater = self.segment_updater.clone();

        let scheduler = global_scheduler();
        let rx = scheduler.submit_task(crate::actor::Priority::High, move || {
            finalize_segment(segment, writer, &segment_updater, &mut delete_cursor)
                .map(|_| vec![0u8]) // success marker
                .map_err(|e| e.encode())
        });
        self.pending_finalize = Some(rx);
    }

    /// Non-blocking poll: check if the pending finalize completed.
    fn poll_pending_finalize(&mut self) {
        if let Some(ref rx) = self.pending_finalize {
            if rx.is_ready() {
                let rx = self.pending_finalize.take().unwrap();
                if let Some(result) = rx.take_value() {
                    if let Err(err_bytes) = result {
                        let err = crate::LucivyError::decode(&err_bytes)
                            .unwrap_or_else(|_| {
                                crate::LucivyError::SystemError("background finalize failed".into())
                            });
                        self.set_error(err);
                    }
                }
            }
        }
    }

    /// Blocking wait for pending finalize. Used by handle_flush (after drain).
    fn wait_pending_finalize(&mut self) -> crate::Result<()> {
        if let Some(rx) = self.pending_finalize.take() {
            let scheduler = global_scheduler();
            match scheduler.wait(rx, "pending_finalize") {
                Ok(_) => Ok(()),
                Err(err_bytes) => {
                    let err = crate::LucivyError::decode(&err_bytes)
                        .unwrap_or_else(|_| {
                            crate::LucivyError::SystemError("background finalize failed".into())
                        });
                    Err(err)
                }
            }
        } else {
            Ok(())
        }
    }

    fn has_open_segment(&self) -> bool {
        self.current.is_some()
    }
}

// ─── Actor creation ─────────────────────────────────────────────────────────

/// Create a GenericActor for document indexation.
///
/// The type parameter `D` is captured by the handler closures (type erasure).
/// Each IndexerActor gets its own FinalizerActor for background finalization.
pub(crate) fn create_indexer_actor<D: Document>(
    segment_updater: SegmentUpdater,
    index: Index,
    mem_budget: usize,
    delete_cursor: DeleteCursor,
    bomb: IndexWriterBomb<D>,
) -> GenericActor {
    // Spawn a dedicated FinalizerActor for this IndexerActor.
    let finalizer_ref = spawn_finalizer_actor();

    let mut actor = GenericActor::new("indexer")
        .with_priority_fn(|state| {
            if let Some(s) = state.get::<IndexerState<crate::LucivyDocument>>() {
                if s.has_open_segment() {
                    return Priority::High;
                }
            }
            Priority::Low
        });

    let indexer_state = IndexerState::<D> {
        segment_updater,
        index,
        mem_budget,
        delete_cursor,
        bomb: Some(bomb),
        current: None,
        pending_error: None,
        finalizer_ref,
        pending_finalize: None,
        docs_since_yield: 0,
    };
    actor.state_mut().insert::<IndexerState<D>>(indexer_state);

    // Docs handler: AddBatch<D> in Envelope.local
    actor.register(TypedHandler::<IndexerDocsMsg, _>::with_priority(
        |state, _msg, _reply, local, _ctx| {
            let batch = local
                .and_then(|l| l.downcast::<AddBatch<D>>().ok())
                .map(|b| *b);
            if let Some(batch) = batch {
                let s = state.get_mut::<IndexerState<D>>().unwrap();
                let did_finalize = s.handle_docs(batch);
                if did_finalize {
                    // Yield after heavy finalize to free the scheduler thread
                    // for other actors (drain, commit, etc.).
                    return ActorStatus::Yield;
                }
            }
            ActorStatus::Continue
        },
        Priority::High,
    ));

    // Flush handler: submit finalize to background, reply when ALL done.
    //
    // handle_flush returns pending finalize receivers. If none, reply
    // immediately. If some, use collect_replies_to on self_ref to get
    // notified when all finalizes complete, then reply.
    actor.register(TypedHandler::<IndexerFlushMsg, _>::with_priority(
        |state, _msg, reply, _local, _ctx| {
            let s = state.get_mut::<IndexerState<D>>().unwrap();
            match s.handle_flush() {
                Err(e) => {
                    if let Some(reply) = reply { reply.send_err(e); }
                }
                Ok(rxs) if rxs.is_empty() => {
                    // No pending finalize — reply immediately.
                    if let Some(reply) = reply { reply.send(IndexerFlushReply); }
                }
                Ok(rxs) => {
                    // Wait for background finalizes via collect_replies_to
                    // on self_ref. When all complete, send the flush reply.
                    let self_ref = state.get::<ActorRef<Envelope>>().unwrap().clone();
                    crate::actor::reply::collect_replies_to(
                        rxs,
                        &self_ref,
                        "indexer_flush_finalize",
                        move |results| {
                            let success = results.iter().all(|r| r.is_ok());
                            let local: Box<dyn std::any::Any + Send> =
                                Box::new((success, reply));
                            Envelope {
                                type_tag: IndexerFinalizeCompleteMsg::type_tag(),
                                payload: IndexerFinalizeCompleteMsg.encode(),
                                reply: None,
                                local: Some(local),
                            }
                        },
                    );
                }
            }
            ActorStatus::Continue
        },
        Priority::Critical,
    ));

    // FinalizeComplete handler: background finalize done, send the flush reply.
    actor.register(TypedHandler::<IndexerFinalizeCompleteMsg, _>::new(
        |_state, _msg, _reply, local, _ctx| {
            let (success, flush_reply): (bool, Option<crate::actor::envelope::ReplyPort>) =
                *local.unwrap().downcast().unwrap();
            if let Some(reply) = flush_reply {
                if success {
                    reply.send(IndexerFlushReply);
                } else {
                    reply.send_err(crate::LucivyError::SystemError(
                        "background finalize failed".into(),
                    ));
                }
            }
            ActorStatus::Continue
        },
    ));

    // Drain handler: reply immediately (FIFO guarantees all prior DocsMsg processed).
    actor.register(TypedHandler::<IndexerDrainMsg, _>::new(
        |_state, _msg, reply, _local, _ctx| {
            if let Some(reply) = reply {
                reply.send(IndexerDrainReply);
            }
            ActorStatus::Continue
        },
    ));

    // Shutdown handler
    actor.register(TypedHandler::<IndexerShutdownMsg, _>::new(
        |state, _msg, _reply, _local, _ctx| {
            let s = state.get_mut::<IndexerState<D>>().unwrap();
            s.handle_shutdown();
            ActorStatus::Stop
        },
    ));

    actor
}
