//! IndexerActor — document indexation via GenericActor.
//!
//! Each IndexerActor receives batches of documents via its mailbox,
//! accumulates them in a segment, and finalizes on Flush or when
//! the memory budget is reached.

use std::any::Any;

use crate::actor::actor_state::ActorState;
use crate::actor::envelope::{type_tag_hash, Envelope, Message, ReplyPort};
use crate::actor::generic_actor::GenericActor;
use crate::actor::handler::TypedHandler;
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
            let _ = self.finalize_current_segment();
        }
    }

    fn handle_flush(&mut self) -> crate::Result<()> {
        if let Some(err) = self.pending_error.take() {
            Err(err)
        } else {
            self.finalize_current_segment()
        }
    }

    fn handle_shutdown(&mut self) {
        let _ = self.finalize_current_segment();
        if let Some(bomb) = self.bomb.take() {
            bomb.defuse();
        }
    }

    fn set_error(&mut self, err: crate::LucivyError) {
        self.pending_error = Some(err);
        drop(self.bomb.take());
    }

    fn finalize_current_segment(&mut self) -> crate::Result<()> {
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

    fn has_open_segment(&self) -> bool {
        self.current.is_some()
    }
}

// ─── Actor creation ─────────────────────────────────────────────────────────

/// Create a GenericActor for document indexation.
///
/// The type parameter `D` is captured by the handler closures (type erasure).
pub(crate) fn create_indexer_actor<D: Document>(
    segment_updater: SegmentUpdater,
    index: Index,
    mem_budget: usize,
    delete_cursor: DeleteCursor,
    bomb: IndexWriterBomb<D>,
) -> GenericActor {
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
