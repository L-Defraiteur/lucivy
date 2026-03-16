//! Directory implementation backed by a [`BlobStore`] with local mmap cache.
//!
//! `BlobDirectory` materializes blobs from a `BlobStore` (database, S3, etc.)
//! into a local temporary directory, then delegates all I/O to a [`StdFsDirectory`]
//! on that cache dir. This gives native mmap performance for reads while keeping
//! the `BlobStore` as the durable source of truth.
//!
//! Flow:
//! - **Open**: `BlobStore.list()` + `load()` → write to cache_dir → `StdFsDirectory`
//! - **Write**: buffer in RAM → flush to cache_dir + `BlobStore.save()`
//! - **Delete**: remove from cache_dir + `BlobStore.delete()`
//! - **Read**: delegate to `StdFsDirectory` (mmap-capable, zero-copy)
//! - **Drop**: cleanup cache_dir

use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use ld_lucivy::directory::error::{DeleteError, OpenReadError, OpenWriteError};
use ld_lucivy::directory::{
    AntiCallToken, Directory, FileHandle, FileSlice, TerminatingWrite, WatchCallback,
    WatchCallbackList, WatchHandle, WritePtr,
};

use crate::blob_store::BlobStore;
use crate::directory::StdFsDirectory;

/// Monotonic counter for unique cache dir names.
static CACHE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Prefix applied to all index names in the BlobStore.
/// Guarantees zero collision with other subsystems (e.g. Sparse uses "Sparse_").
/// The caller passes a bare name like "Product", the BlobStore sees "Lucivy_Product".
pub const BLOB_PREFIX: &str = "Lucivy_";

/// A [`Directory`] backed by a [`BlobStore`] with a local filesystem cache.
///
/// At construction, all blobs are materialized from the store into a temporary
/// directory. Reads go through the local cache (mmap-capable). Writes and
/// deletes are applied to both the cache and the store.
///
/// The cache directory is reference-counted and cleaned up when the last
/// `BlobDirectory` sharing it is dropped (lucivy's lock system clones
/// the directory, so we must not delete the cache prematurely).
pub struct BlobDirectory<S: BlobStore> {
    store: Arc<S>,
    /// Prefixed index name used for BlobStore keys (e.g. "Lucivy_Product").
    prefixed_name: String,
    inner: StdFsDirectory,
    cache_dir: Arc<PathBuf>,
    watch_router: Arc<RwLock<WatchCallbackList>>,
}

impl<S: BlobStore> BlobDirectory<S> {
    /// Create a new `BlobDirectory`.
    ///
    /// `index_name` is the bare name (e.g. "Product"). The `BLOB_PREFIX` is
    /// applied automatically for BlobStore keys.
    ///
    /// `cache_base` is the root directory for local caches. Layout:
    /// `{cache_base}/{pid}/{Lucivy_name}_{seq}/` — PID isolates processes,
    /// atomic counter isolates threads.
    ///
    /// Materializes all existing blobs from the store into a local cache directory.
    /// If no blobs exist (fresh index), the cache dir is created empty.
    pub fn new(store: Arc<S>, index_name: impl Into<String>, cache_base: &Path) -> io::Result<Self> {
        let index_name = index_name.into();
        let prefixed_name = format!("{BLOB_PREFIX}{index_name}");
        let seq = CACHE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let cache_dir = cache_base
            .join(format!("{pid}"))
            .join(format!("{prefixed_name}_{seq}"));

        // Clean up any stale cache dir, then create fresh
        let _ = std::fs::remove_dir_all(&cache_dir);
        std::fs::create_dir_all(&cache_dir)?;

        // Materialize all blobs from the store
        let files = store.list(&prefixed_name)?;
        for file_name in &files {
            let data = store.load(&prefixed_name, file_name)?;
            let file_path = cache_dir.join(file_name);
            if let Some(parent) = file_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&file_path, &data)?;
        }

        let inner = StdFsDirectory::open(&cache_dir)?;

        Ok(Self {
            store,
            prefixed_name,
            inner,
            cache_dir: Arc::new(cache_dir),
            watch_router: Arc::new(RwLock::new(WatchCallbackList::default())),
        })
    }

    /// Convert a Path to a file_name string for the blob store.
    fn file_name(path: &Path) -> String {
        path.to_string_lossy().to_string()
    }
}

impl<S: BlobStore> Clone for BlobDirectory<S> {
    fn clone(&self) -> Self {
        Self {
            store: self.store.clone(),
            prefixed_name: self.prefixed_name.clone(),
            inner: self.inner.clone(),
            cache_dir: self.cache_dir.clone(),
            watch_router: self.watch_router.clone(),
        }
    }
}

impl<S: BlobStore> std::fmt::Debug for BlobDirectory<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "BlobDirectory({}, cache={:?})", self.prefixed_name, self.cache_dir.as_ref())
    }
}

/// Writer that writes to the local cache file AND saves to the blob store on flush.
struct BlobWriter<S: BlobStore> {
    store: Arc<S>,
    prefixed_name: String,
    file_name: String,
    cache_path: PathBuf,
    buffer: Vec<u8>,
    is_flushed: bool,
}

impl<S: BlobStore> Write for BlobWriter<S> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.is_flushed = false;
        self.buffer.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        // Write to local cache (for mmap reads)
        std::fs::write(&self.cache_path, &self.buffer)?;
        // Sync to blob store (for durability)
        self.store
            .save(&self.prefixed_name, &self.file_name, &self.buffer)?;
        self.is_flushed = true;
        Ok(())
    }
}

impl<S: BlobStore> TerminatingWrite for BlobWriter<S> {
    fn terminate_ref(&mut self, _: AntiCallToken) -> io::Result<()> {
        self.flush()
    }
}

impl<S: BlobStore> Drop for BlobWriter<S> {
    fn drop(&mut self) {
        if !self.is_flushed && !self.buffer.is_empty() {
            eprintln!(
                "Warning: BlobWriter for {}/{} dropped without flushing. Data may be lost.",
                self.prefixed_name, self.file_name
            );
        }
    }
}

impl<S: BlobStore> Directory for BlobDirectory<S> {
    fn get_file_handle(&self, path: &Path) -> Result<Arc<dyn FileHandle>, OpenReadError> {
        // Delegate to inner StdFsDirectory — reads from local cache (mmap-capable)
        self.inner.get_file_handle(path)
    }

    fn open_read(&self, path: &Path) -> Result<FileSlice, OpenReadError> {
        // Delegate to inner — reads from local cache
        self.inner.open_read(path)
    }

    fn open_write(&self, path: &Path) -> Result<WritePtr, OpenWriteError> {
        let fname = Self::file_name(path);
        let cache_path = self.cache_dir.as_ref().join(path);

        // WORM semantics: fail if file already exists in cache
        if cache_path.exists() {
            return Err(OpenWriteError::FileAlreadyExists(cache_path));
        }
        if let Some(parent) = cache_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| OpenWriteError::IoError {
                io_error: Arc::new(e),
                filepath: cache_path.clone(),
            })?;
        }

        let writer = BlobWriter {
            store: self.store.clone(),
            prefixed_name: self.prefixed_name.clone(),
            file_name: fname,
            cache_path,
            buffer: Vec::new(),
            is_flushed: true,
        };
        Ok(BufWriter::new(Box::new(writer)))
    }

    fn delete(&self, path: &Path) -> Result<(), DeleteError> {
        let fname = Self::file_name(path);

        // Delete from local cache
        self.inner.delete(path)?;

        // Delete from blob store (best effort — cache is authoritative during runtime)
        let _ = self.store.delete(&self.prefixed_name, &fname);
        Ok(())
    }

    fn exists(&self, path: &Path) -> Result<bool, OpenReadError> {
        // Check local cache (authoritative during runtime)
        self.inner.exists(path)
    }

    fn atomic_read(&self, path: &Path) -> Result<Vec<u8>, OpenReadError> {
        // Read from local cache
        self.inner.atomic_read(path)
    }

    fn atomic_write(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        // Write to local cache
        self.inner.atomic_write(path, data)?;

        // Sync to blob store
        let fname = Self::file_name(path);
        self.store.save(&self.prefixed_name, &fname, data)?;

        // Notify watchers on meta.json write (commit point)
        if path == Path::new("meta.json") {
            if let Ok(router) = self.watch_router.read() {
                let _ = router.broadcast();
            }
        }
        Ok(())
    }

    fn watch(&self, watch_callback: WatchCallback) -> ld_lucivy::Result<WatchHandle> {
        Ok(self
            .watch_router
            .write()
            .map_err(|_| {
                ld_lucivy::LucivyError::SystemError("watch lock poisoned".to_string())
            })?
            .subscribe(watch_callback))
    }

    fn sync_directory(&self) -> io::Result<()> {
        // Local cache is already on disk. BlobStore handles its own durability.
        Ok(())
    }
}

impl<S: BlobStore> Drop for BlobDirectory<S> {
    fn drop(&mut self) {
        // Only cleanup if we are the last reference to this cache dir.
        // Lucivy's lock system clones the directory (via box_clone), so
        // multiple BlobDirectory instances may share the same cache_dir.
        if Arc::strong_count(&self.cache_dir) == 1 {
            let _ = std::fs::remove_dir_all(self.cache_dir.as_ref());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blob_store::MemBlobStore;
    use crate::handle::LucivyHandle;
    use crate::query::SchemaConfig;

    fn cache_base() -> PathBuf {
        std::env::temp_dir().join("lucivy_test_cache")
    }

    fn test_config() -> SchemaConfig {
        serde_json::from_str(
            r#"{"fields": [{"name": "body", "type": "text", "stored": true}]}"#,
        )
        .unwrap()
    }

    fn insert_docs(handle: &LucivyHandle, count: u64) {
        let body = handle.field("body").unwrap();
        let nid = handle.field("_node_id").unwrap();
        let mut guard = handle.writer.lock().unwrap();
        let w = guard.as_mut().unwrap();
        for i in 0..count {
            let text = format!("document number {i} about rust programming");
            let mut doc = ld_lucivy::LucivyDocument::new();
            doc.add_u64(nid, i);
            doc.add_text(body, &text);
            w.add_document(doc).unwrap();
        }
        w.commit().unwrap();
    }

    #[test]
    fn test_blob_directory_create_and_search() {
        let store = Arc::new(MemBlobStore::new());
        let dir = BlobDirectory::new(store.clone(), "test_idx", &cache_base()).unwrap();
        let config = test_config();

        let handle = LucivyHandle::create(dir, &config).unwrap();
        insert_docs(&handle, 5);
        handle.reader.reload().unwrap();

        assert_eq!(handle.reader.searcher().num_docs(), 5);

        // Verify blobs were synced to the store
        let files = store.list("Lucivy_test_idx").unwrap();
        assert!(files.len() > 1, "should have multiple blobs: {:?}", files);
        assert!(
            store.exists("Lucivy_test_idx", "meta.json").unwrap(),
            "meta.json should exist in store"
        );
    }

    #[test]
    fn test_blob_directory_create_close_reopen() {
        let store = Arc::new(MemBlobStore::new());
        let config = test_config();

        // Phase 1: create + insert + close
        {
            let dir = BlobDirectory::new(store.clone(), "reopen_idx", &cache_base()).unwrap();
            let handle = LucivyHandle::create(dir, &config).unwrap();
            insert_docs(&handle, 10);
            handle.close().unwrap();
            // handle + BlobDirectory dropped here → cache_dir cleaned up
        }

        // Phase 2: reopen from store — blobs re-materialized to fresh cache_dir
        let dir = BlobDirectory::new(store.clone(), "reopen_idx", &cache_base()).unwrap();
        let handle = LucivyHandle::open(dir).unwrap();
        handle.reader.reload().unwrap();
        assert_eq!(
            handle.reader.searcher().num_docs(),
            10,
            "all 10 docs should survive close/reopen via BlobDirectory"
        );
    }

    #[test]
    fn test_blob_directory_worm_semantics() {
        let store = Arc::new(MemBlobStore::new());
        let dir = BlobDirectory::new(store, "worm_idx", &cache_base()).unwrap();

        // Write a file
        let path = Path::new("test.bin");
        {
            let mut writer = dir.open_write(path).unwrap();
            writer.write_all(b"hello").unwrap();
            writer.flush().unwrap();
        }

        // Try to write again — should fail (WORM)
        assert!(dir.open_write(path).is_err());

        // Read back
        let data = dir.atomic_read(path).unwrap();
        assert_eq!(data, b"hello");

        // Delete then re-write should succeed
        dir.delete(path).unwrap();
        assert!(dir.open_write(path).is_ok());
    }

    #[test]
    fn test_blob_directory_search_after_reopen() {
        let store = Arc::new(MemBlobStore::new());
        let config = test_config();

        // Create and populate
        {
            let dir = BlobDirectory::new(store.clone(), "search_idx", &cache_base()).unwrap();
            let handle = LucivyHandle::create(dir, &config).unwrap();
            let body = handle.field("body").unwrap();
            let nid = handle.field("_node_id").unwrap();
            {
                let mut guard = handle.writer.lock().unwrap();
                let w = guard.as_mut().unwrap();
                let texts = [
                    "rust is a systems programming language",
                    "python is great for data science",
                    "javascript runs in the browser",
                ];
                for (i, text) in texts.iter().enumerate() {
                    let mut doc = ld_lucivy::LucivyDocument::new();
                    doc.add_u64(nid, i as u64);
                    doc.add_text(body, text);
                    w.add_document(doc).unwrap();
                }
                w.commit().unwrap();
            }
            handle.close().unwrap();
        }

        // Reopen and search
        let dir = BlobDirectory::new(store, "search_idx", &cache_base()).unwrap();
        let handle = LucivyHandle::open(dir).unwrap();
        handle.reader.reload().unwrap();

        let query_config: crate::query::QueryConfig =
            serde_json::from_str(r#"{"type": "term", "field": "body", "value": "rust"}"#).unwrap();
        let query = crate::query::build_query(
            &query_config,
            &handle.schema,
            &handle.index,
            None,
        )
        .unwrap();

        let searcher = handle.reader.searcher();
        let collector = ld_lucivy::collector::TopDocs::with_limit(10).order_by_score();
        let results = searcher.search(&*query, &collector).unwrap();
        assert_eq!(results.len(), 1, "should find 1 doc matching 'rust'");
    }

    #[test]
    fn test_blob_directory_multiple_indexes_isolated() {
        let store = Arc::new(MemBlobStore::new());
        let config = test_config();

        let dir1 = BlobDirectory::new(store.clone(), "idx_a", &cache_base()).unwrap();
        let dir2 = BlobDirectory::new(store.clone(), "idx_b", &cache_base()).unwrap();

        let h1 = LucivyHandle::create(dir1, &config).unwrap();
        let h2 = LucivyHandle::create(dir2, &config).unwrap();

        insert_docs(&h1, 3);
        insert_docs(&h2, 7);

        h1.reader.reload().unwrap();
        h2.reader.reload().unwrap();

        assert_eq!(h1.reader.searcher().num_docs(), 3);
        assert_eq!(h2.reader.searcher().num_docs(), 7);
    }

    #[test]
    fn test_blob_directory_survives_cache_cleanup() {
        // Verify that data persists in the BlobStore even after cache_dir is deleted
        let store = Arc::new(MemBlobStore::new());
        let config = test_config();

        // Create, insert, close (cache_dir cleaned on drop)
        {
            let dir = BlobDirectory::new(store.clone(), "survive_idx", &cache_base()).unwrap();
            let handle = LucivyHandle::create(dir, &config).unwrap();
            insert_docs(&handle, 20);
            handle.close().unwrap();
        }

        // Store should still have all blobs
        let files = store.list("Lucivy_survive_idx").unwrap();
        assert!(files.len() > 1, "store should retain blobs after cache cleanup");
        assert!(store.exists("Lucivy_survive_idx", "meta.json").unwrap());

        // Reopen — materializes from store to fresh cache
        let dir = BlobDirectory::new(store, "survive_idx", &cache_base()).unwrap();
        let handle = LucivyHandle::open(dir).unwrap();
        handle.reader.reload().unwrap();
        assert_eq!(handle.reader.searcher().num_docs(), 20);
    }

    #[test]
    fn test_blob_directory_multiple_commits() {
        let store = Arc::new(MemBlobStore::new());
        let config = test_config();

        let dir = BlobDirectory::new(store.clone(), "multi_commit", &cache_base()).unwrap();
        let handle = LucivyHandle::create(dir, &config).unwrap();
        let body = handle.field("body").unwrap();
        let nid = handle.field("_node_id").unwrap();

        // Multiple commits → multiple segments
        for batch in 0u64..5 {
            let mut guard = handle.writer.lock().unwrap();
            let w = guard.as_mut().unwrap();
            for i in 0u64..10 {
                let id = batch * 10 + i;
                let mut doc = ld_lucivy::LucivyDocument::new();
                doc.add_u64(nid, id);
                doc.add_text(body, &format!("batch {batch} doc {i}"));
                w.add_document(doc).unwrap();
            }
            w.commit().unwrap();
        }
        handle.reader.reload().unwrap();
        assert_eq!(handle.reader.searcher().num_docs(), 50);

        // Close and reopen — all segments should be in the store
        handle.close().unwrap();
        drop(handle);

        let dir = BlobDirectory::new(store, "multi_commit", &cache_base()).unwrap();
        let handle = LucivyHandle::open(dir).unwrap();
        handle.reader.reload().unwrap();
        assert_eq!(handle.reader.searcher().num_docs(), 50);
    }
}
