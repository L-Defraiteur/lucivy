use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::schema::document::Document;
use crate::LucivyDocument;

#[derive(Clone)]
pub(crate) struct IndexWriterStatus<D: Document = LucivyDocument> {
    inner: Arc<Inner<D>>,
}

impl<D: Document> IndexWriterStatus<D> {
    pub fn new() -> Self {
        IndexWriterStatus {
            inner: Arc::new(Inner {
                is_alive: AtomicBool::new(true),
                _phantom: std::marker::PhantomData,
            }),
        }
    }

    /// Returns true iff the index writer is alive.
    pub fn is_alive(&self) -> bool {
        self.inner.is_alive.load(Ordering::Relaxed)
    }

    /// Create an index writer bomb.
    /// If dropped, the index writer status will be killed.
    pub(crate) fn create_bomb(&self) -> IndexWriterBomb<D> {
        IndexWriterBomb {
            inner: Some(self.inner.clone()),
        }
    }
}

struct Inner<D: Document> {
    is_alive: AtomicBool,
    _phantom: std::marker::PhantomData<D>,
}

impl<D: Document> Inner<D> {
    fn kill(&self) {
        self.is_alive.store(false, Ordering::Relaxed);
    }
}

/// If dropped, the index writer will be killed.
/// To prevent this, clients can call `.defuse()`.
pub(crate) struct IndexWriterBomb<D: Document> {
    inner: Option<Arc<Inner<D>>>,
}

impl<D: Document> IndexWriterBomb<D> {
    /// Defuses the bomb.
    pub fn defuse(mut self) {
        self.inner = None;
    }
}

impl<D: Document> Drop for IndexWriterBomb<D> {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.take() {
            inner.kill();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::mem;

    use super::IndexWriterStatus;
    use crate::LucivyDocument;

    #[test]
    fn test_bomb_goes_boom() {
        let status: IndexWriterStatus<LucivyDocument> = IndexWriterStatus::new();
        assert!(status.is_alive());
        let bomb = status.create_bomb();
        assert!(status.is_alive());
        mem::drop(bomb);
        // boom!
        assert!(!status.is_alive());
    }

    #[test]
    fn test_bomb_defused() {
        let status: IndexWriterStatus<LucivyDocument> = IndexWriterStatus::new();
        assert!(status.is_alive());
        let bomb = status.create_bomb();
        bomb.defuse();
        assert!(status.is_alive());
    }
}
