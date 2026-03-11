use crate::actor::{Actor, ActorStatus, Priority, Reply};
use crate::index::{Index, Segment};
use crate::indexer::delete_queue::DeleteCursor;
use crate::indexer::index_writer::{finalize_segment, MARGIN_IN_BYTES};
use crate::indexer::index_writer_status::IndexWriterBomb;
use crate::indexer::segment_updater::SegmentUpdater;
use crate::indexer::{AddBatch, SegmentWriter};
use crate::schema::document::Document;

/// Messages reçus par un IndexerActor.
pub(crate) enum IndexerMsg<D: Document> {
    /// Batch de documents à indexer.
    Docs(AddBatch<D>),
    /// Flush le segment en cours et répondre quand c'est fait.
    Flush(Reply<crate::Result<()>>),
    /// Arrêt propre.
    Shutdown,
}

/// Segment en cours d'écriture.
struct SegmentInProgress {
    segment: Segment,
    writer: SegmentWriter,
}

/// Acteur d'indexation. Remplace `worker_loop`.
///
/// Chaque IndexerActor reçoit des batches de documents via sa mailbox FIFO,
/// les accumule dans un segment en cours, et finalise le segment sur Flush
/// ou quand le budget mémoire est atteint.
///
/// La FIFO de la mailbox garantit que tous les Docs envoyés avant un Flush
/// sont traités avant le Flush — plus besoin du hack `try_recv` drain.
pub(crate) struct IndexerActor<D: Document> {
    segment_updater: SegmentUpdater,
    index: Index,
    mem_budget: usize,
    delete_cursor: DeleteCursor,
    bomb: Option<IndexWriterBomb<D>>,
    /// Segment en cours d'écriture (None si idle).
    current: Option<SegmentInProgress>,
    /// Erreur capturée pendant handle_docs, renvoyée au prochain Flush.
    pending_error: Option<crate::LucivyError>,
}

impl<D: Document> IndexerActor<D> {
    pub fn new(
        segment_updater: SegmentUpdater,
        index: Index,
        mem_budget: usize,
        delete_cursor: DeleteCursor,
        bomb: IndexWriterBomb<D>,
    ) -> Self {
        IndexerActor {
            segment_updater,
            index,
            mem_budget,
            delete_cursor,
            bomb: Some(bomb),
            current: None,
            pending_error: None,
        }
    }

    fn handle_docs(&mut self, batch: AddBatch<D>) -> ActorStatus {
        if batch.is_empty() || self.pending_error.is_some() {
            return ActorStatus::Continue;
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
                        return ActorStatus::Continue;
                    }
                };
                self.current = Some(SegmentInProgress { segment, writer });
                self.current.as_mut().unwrap()
            }
        };

        for doc in batch {
            if let Err(e) = current.writer.add_document(doc) {
                // Drop le segment en cours — les données sont corrompues.
                self.current.take();
                self.set_error(e);
                return ActorStatus::Continue;
            }
        }

        // Vérifier le budget mémoire.
        if current.writer.mem_usage() >= self.mem_budget - MARGIN_IN_BYTES {
            info!(
                "Buffer limit reached, flushing segment with maxdoc={}.",
                current.writer.max_doc()
            );
            let _ = self.finalize_current_segment();
        }

        ActorStatus::Continue
    }

    fn handle_flush(&mut self, reply: Reply<crate::Result<()>>) -> ActorStatus {
        // Tous les Docs envoyés avant ce Flush ont déjà été traités
        // par des appels handle_docs() précédents (FIFO garanti).
        let result = if let Some(err) = self.pending_error.take() {
            Err(err)
        } else {
            self.finalize_current_segment()
        };
        reply.send(result);
        ActorStatus::Continue
    }

    fn handle_shutdown(&mut self) -> ActorStatus {
        let _ = self.finalize_current_segment();
        if let Some(bomb) = self.bomb.take() {
            bomb.defuse();
        }
        ActorStatus::Stop
    }

    /// Stocke une erreur fatale et tue le writer status (via bomb drop).
    fn set_error(&mut self, err: crate::LucivyError) {
        self.pending_error = Some(err);
        // Drop la bomb → IndexWriterStatus::is_alive() retourne false
        // → les prochains send_add_documents_batch échoueront immédiatement.
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
}

impl<D: Document> Actor for IndexerActor<D> {
    type Msg = IndexerMsg<D>;

    fn name(&self) -> &'static str {
        "indexer"
    }

    fn handle(&mut self, msg: IndexerMsg<D>) -> ActorStatus {
        match msg {
            IndexerMsg::Docs(batch) => self.handle_docs(batch),
            IndexerMsg::Flush(reply) => self.handle_flush(reply),
            IndexerMsg::Shutdown => self.handle_shutdown(),
        }
    }

    fn priority(&self) -> Priority {
        if self.current.is_some() {
            Priority::High // segment ouvert = mémoire allouée
        } else {
            Priority::Low // idle
        }
    }
}
