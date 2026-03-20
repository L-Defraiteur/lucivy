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
        |_state, _msg, reply, local| {
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
}

impl<D: Document> IndexerState<D> {
    fn handle_docs(&mut self, batch: AddBatch<D>) {
        if batch.is_empty() || self.pending_error.is_some() {
            return;
        }

        let current = match &mut self.current {
            Some(c) => c,
            None => {
                self.delete_cursor.skip_to(batch[0].opstamp);
                let segment = self.index.new_segment();
                let writer = match SegmentWriter::for_segment(self.mem_budget, segment.clone()) {
                    Ok(w) => w,
                    Err(e) => {
                        self.set_error(e);
                        return;
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
                return;
            }
        }

        if current.writer.mem_usage() >= self.mem_budget - MARGIN_IN_BYTES {
            info!(
                "Buffer limit reached, flushing segment with maxdoc={}.",
                current.writer.max_doc()
            );
            let _ = self.finalize_current_segment_background();
        }
    }

    fn handle_flush(&mut self) -> crate::Result<()> {
        if let Some(err) = self.pending_error.take() {
            return Err(err);
        }
        // Finalize the current segment (blocking — it's a Flush).
        self.finalize_current_segment_blocking()?;
        // Wait for any background finalize to complete.
        self.wait_pending_finalize()
    }

    fn handle_shutdown(&mut self) {
        let _ = self.finalize_current_segment_blocking();
        let _ = self.wait_pending_finalize();
        if let Some(bomb) = self.bomb.take() {
            bomb.defuse();
        }
    }

    fn set_error(&mut self, err: crate::LucivyError) {
        self.pending_error = Some(err);
        drop(self.bomb.take());
    }

    /// Finalize the current segment in background: send work to FinalizerActor,
    /// start a new segment immediately. Pipeline depth = 2.
    fn finalize_current_segment_background(&mut self) -> crate::Result<()> {
        let current = match self.current.take() {
            Some(c) => c,
            None => return Ok(()),
        };
        if !self.segment_updater.is_alive() {
            return Ok(());
        }

        // Wait for any previous background finalize first (at most 1 pending).
        self.wait_pending_finalize()?;

        // Clone the delete cursor for the background work.
        // The IndexerActor's own cursor will advance with skip_to() when the
        // next segment starts.
        let cursor_clone = self.delete_cursor.clone();

        let work = FinalizeWork {
            segment: current.segment,
            writer: current.writer,
            delete_cursor: cursor_clone,
            segment_updater: self.segment_updater.clone(),
        };

        let (env, rx) = FinalizeMsg.into_request_with_local(work);
        if self.finalizer_ref.send(env).is_ok() {
            self.pending_finalize = Some(rx);
        } else {
            return Err(crate::LucivyError::SystemError(
                "finalizer actor channel closed".into(),
            ));
        }

        Ok(())
    }

    /// Finalize the current segment synchronously (for Flush/Shutdown).
    fn finalize_current_segment_blocking(&mut self) -> crate::Result<()> {
        if let Some(current) = self.current.take() {
            if self.segment_updater.is_alive() {
                finalize_segment(
                    current.segment,
                    current.writer,
                    &self.segment_updater,
                    &mut self.delete_cursor,
                )?;
            }
        }
        Ok(())
    }

    /// Wait for a pending background finalize to complete.
    fn wait_pending_finalize(&mut self) -> crate::Result<()> {
        if let Some(rx) = self.pending_finalize.take() {
            let scheduler = global_scheduler();
            // wait_cooperative returns Result<Vec<u8>, Vec<u8>>:
            // Ok = FinalizeReply encoded, Err = LucivyError encoded.
            match rx.wait_cooperative(|| scheduler.run_one_step()) {
                Ok(_success_bytes) => Ok(()),
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
    };
    actor.state_mut().insert::<IndexerState<D>>(indexer_state);

    // Docs handler: AddBatch<D> in Envelope.local
    actor.register(TypedHandler::<IndexerDocsMsg, _>::with_priority(
        |state, _msg, _reply, local| {
            let batch = local
                .and_then(|l| l.downcast::<AddBatch<D>>().ok())
                .map(|b| *b);
            if let Some(batch) = batch {
                let s = state.get_mut::<IndexerState<D>>().unwrap();
                s.handle_docs(batch);
            }
            ActorStatus::Continue
        },
        Priority::High,
    ));

    // Flush handler: reply with Ok/Err
    actor.register(TypedHandler::<IndexerFlushMsg, _>::with_priority(
        |state, _msg, reply, _local| {
            let s = state.get_mut::<IndexerState<D>>().unwrap();
            let result = s.handle_flush();
            if let Some(reply) = reply {
                match result {
                    Ok(()) => reply.send(IndexerFlushReply),
                    Err(e) => reply.send_err(e),
                }
            }
            ActorStatus::Continue
        },
        Priority::Critical,
    ));

    // Shutdown handler
    actor.register(TypedHandler::<IndexerShutdownMsg, _>::new(
        |state, _msg, _reply, _local| {
            let s = state.get_mut::<IndexerState<D>>().unwrap();
            s.handle_shutdown();
            ActorStatus::Stop
        },
    ));

    actor
}
