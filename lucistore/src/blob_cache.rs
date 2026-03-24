//! BlobCache — local filesystem cache backed by a BlobStore.
//!
//! Pattern "DB stocke, mmap sert":
//! - All blobs are materialized from the BlobStore to a local temp directory
//! - Reads go through the local cache (mmap-capable, zero-copy)
//! - Writes are write-through: local cache + BlobStore
//! - Cache is cleaned up on drop
//!
//! Consumers (lucivy, sparse_vector, etc.) wrap BlobCache to implement
//! their own I/O traits (Directory, etc.).

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::blob_store::BlobStore;

/// Monotonic counter for unique cache dir names.
static CACHE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A local filesystem cache backed by a [`BlobStore`].
///
/// At construction, all blobs are materialized from the store into a temporary
/// directory. Reads go through the local cache. Writes and deletes are applied
/// to both the cache and the store (write-through).
///
/// The cache directory is reference-counted (via Arc) and cleaned up when
/// the last clone is dropped.
pub struct BlobCache<S: BlobStore> {
    store: Arc<S>,
    /// Prefixed name used for BlobStore keys.
    prefixed_name: String,
    /// Local cache directory path.
    cache_dir: Arc<CacheDir>,
}

/// RAII wrapper for the cache directory — removes it on drop.
struct CacheDir {
    path: PathBuf,
}

impl Drop for CacheDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

impl<S: BlobStore> BlobCache<S> {
    /// Create a new BlobCache, materializing all existing blobs.
    ///
    /// - `store`: the blob store backend
    /// - `prefix`: namespace prefix (e.g. "Lucivy_", "Sparse_")
    /// - `name`: index name (e.g. "Product")
    /// - `cache_base`: root directory for local caches
    ///
    /// Layout: `{cache_base}/{pid}/{prefix}{name}_{seq}/`
    pub fn new(
        store: Arc<S>,
        prefix: &str,
        name: &str,
        cache_base: &Path,
    ) -> io::Result<Self> {
        let prefixed_name = format!("{prefix}{name}");
        let seq = CACHE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let cache_path = cache_base
            .join(format!("{pid}"))
            .join(format!("{prefixed_name}_{seq}"));

        // Clean up any stale cache, then create fresh.
        let _ = std::fs::remove_dir_all(&cache_path);
        std::fs::create_dir_all(&cache_path)?;

        let cache = Self {
            store,
            prefixed_name,
            cache_dir: Arc::new(CacheDir { path: cache_path }),
        };

        // Materialize all existing blobs.
        cache.materialize()?;

        Ok(cache)
    }

    /// Path to the local cache directory.
    pub fn cache_path(&self) -> &Path {
        &self.cache_dir.path
    }

    /// Access the underlying BlobStore.
    pub fn store(&self) -> &S {
        &self.store
    }

    /// The prefixed name used for BlobStore keys.
    pub fn prefixed_name(&self) -> &str {
        &self.prefixed_name
    }

    /// Materialize all blobs from the store into the local cache.
    ///
    /// Downloads every blob and writes it to the cache directory.
    /// Skips lock files.
    pub fn materialize(&self) -> io::Result<()> {
        let files = self.store.list(&self.prefixed_name)?;
        for file_name in &files {
            if file_name.contains(".lock") {
                continue;
            }
            let data = self.store.load(&self.prefixed_name, file_name)?;
            let local_path = self.cache_dir.path.join(file_name);
            std::fs::write(&local_path, &data)?;
        }
        Ok(())
    }

    /// Write data to both the local cache and the BlobStore (write-through).
    pub fn write_through(&self, file_name: &str, data: &[u8]) -> io::Result<()> {
        // Write to local cache.
        let local_path = self.cache_dir.path.join(file_name);
        std::fs::write(&local_path, data)?;
        // Write to store.
        self.store.save(&self.prefixed_name, file_name, data)
    }

    /// Delete from both the local cache and the BlobStore.
    pub fn delete_through(&self, file_name: &str) -> io::Result<()> {
        let local_path = self.cache_dir.path.join(file_name);
        let _ = std::fs::remove_file(&local_path);
        self.store.delete(&self.prefixed_name, file_name)
    }

    /// Read a file from the local cache (fast path).
    pub fn read_cached(&self, file_name: &str) -> io::Result<Vec<u8>> {
        std::fs::read(self.cache_dir.path.join(file_name))
    }

    /// Check if a file exists in the local cache.
    pub fn exists_cached(&self, file_name: &str) -> bool {
        self.cache_dir.path.join(file_name).exists()
    }

    /// List files in the local cache.
    pub fn list_cached(&self) -> io::Result<Vec<String>> {
        let mut files = Vec::new();
        for entry in std::fs::read_dir(&self.cache_dir.path)? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                files.push(entry.file_name().to_string_lossy().to_string());
            }
        }
        Ok(files)
    }
}

impl<S: BlobStore> Clone for BlobCache<S> {
    fn clone(&self) -> Self {
        Self {
            store: Arc::clone(&self.store),
            prefixed_name: self.prefixed_name.clone(),
            cache_dir: Arc::clone(&self.cache_dir),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blob_store::MemBlobStore;

    #[test]
    fn test_blob_cache_roundtrip() {
        let store = Arc::new(MemBlobStore::new());
        // Pre-populate the store.
        store.save("Test_idx", "meta.json", b"{\"segments\":[]}").unwrap();
        store.save("Test_idx", "data.bin", b"\x01\x02\x03").unwrap();

        let tmp = std::env::temp_dir().join("lucistore_blob_cache_test");
        let cache = BlobCache::new(store.clone(), "Test_", "idx", &tmp).unwrap();

        // Files should be materialized.
        assert!(cache.exists_cached("meta.json"));
        assert!(cache.exists_cached("data.bin"));
        assert_eq!(cache.read_cached("meta.json").unwrap(), b"{\"segments\":[]}");

        // Write-through.
        cache.write_through("new.bin", b"hello").unwrap();
        assert_eq!(cache.read_cached("new.bin").unwrap(), b"hello");
        assert_eq!(store.load("Test_idx", "new.bin").unwrap(), b"hello");

        // Delete-through.
        cache.delete_through("data.bin").unwrap();
        assert!(!cache.exists_cached("data.bin"));
        assert!(!store.exists("Test_idx", "data.bin").unwrap());

        // List.
        let files = cache.list_cached().unwrap();
        assert!(files.contains(&"meta.json".to_string()));
        assert!(files.contains(&"new.bin".to_string()));
        assert!(!files.contains(&"data.bin".to_string()));
    }

    #[test]
    fn test_blob_cache_empty_store() {
        let store = Arc::new(MemBlobStore::new());
        let tmp = std::env::temp_dir().join("lucistore_blob_cache_empty");
        let cache = BlobCache::new(store, "X_", "empty", &tmp).unwrap();
        assert!(cache.list_cached().unwrap().is_empty());
    }

    #[test]
    fn test_blob_cache_clone_shares_dir() {
        let store = Arc::new(MemBlobStore::new());
        let tmp = std::env::temp_dir().join("lucistore_blob_cache_clone");
        let cache1 = BlobCache::new(store, "C_", "test", &tmp).unwrap();
        cache1.write_through("f.bin", b"data").unwrap();

        let cache2 = cache1.clone();
        assert_eq!(cache2.read_cached("f.bin").unwrap(), b"data");
        assert_eq!(cache1.cache_path(), cache2.cache_path());
    }
}
