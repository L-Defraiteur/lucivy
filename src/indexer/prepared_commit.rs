use super::IndexWriter;
use crate::schema::document::Document;
use crate::{Opstamp, LucivyDocument};

/// A prepared commit
pub struct PreparedCommit<'a, D: Document = LucivyDocument> {
    index_writer: &'a mut IndexWriter<D>,
    payload: Option<String>,
    opstamp: Opstamp,
}

impl<'a, D: Document> PreparedCommit<'a, D> {
    pub(crate) fn new(index_writer: &'a mut IndexWriter<D>, opstamp: Opstamp) -> Self {
        Self {
            index_writer,
            payload: None,
            opstamp,
        }
    }

    /// Returns the opstamp associated with the prepared commit.
    pub fn opstamp(&self) -> Opstamp {
        self.opstamp
    }

    /// Adds an arbitrary payload to the commit.
    pub fn set_payload(&mut self, payload: &str) {
        self.payload = Some(payload.to_string())
    }

    /// Rollbacks any change.
    pub fn abort(self) -> crate::Result<Opstamp> {
        self.index_writer.rollback()
    }

    /// Proceeds to commit. Rebuilds suffix FSTs for any deferred segments.
    ///
    /// This flushes deletes, saves metas, and runs garbage collection.
    pub fn commit(self) -> crate::Result<Opstamp> {
        info!("committing {}", self.opstamp);
        self.index_writer
            .segment_updater()
            .schedule_commit_with_rebuild(self.opstamp, self.payload, true)
    }

    /// Fast commit: persist but skip suffix FST rebuild.
    ///
    /// Use during bulk indexation. Deferred FSTs are rebuilt on next
    /// regular `commit()` or on-demand when a segment is loaded for search.
    pub fn commit_fast(self) -> crate::Result<Opstamp> {
        info!("commit_fast {}", self.opstamp);
        self.index_writer
            .segment_updater()
            .schedule_commit_with_rebuild(self.opstamp, self.payload, false)
    }
}
