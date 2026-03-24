//! Generic blob storage trait for persisting data to external backends.
//!
//! This trait abstracts file-level storage so that indexes can be backed
//! by a database, S3, or any other blob store — not just the local filesystem.
//!
//! Implementations:
//! - [`MemBlobStore`]: in-memory store for testing
//! - (external) `CypherBlobStore`: rag3db `_index_blobs` table
//! - (external) `PostgresBlobStore`: Postgres bytea columns
//! - (external) `S3BlobStore`: S3-compatible object storage

use std::collections::HashMap;
use std::io;
use std::sync::{Arc, RwLock};

/// Trait for blob storage backends.
///
/// Each blob is identified by `(index_name, file_name)`.
/// - `index_name`: identifies which index (e.g. "Product", "Article_Index")
/// - `file_name`: identifies which file within the index (e.g. "meta.json", "{uuid}.idx")
pub trait BlobStore: Send + Sync + 'static {
    /// Load a blob. Returns `NotFound` if it doesn't exist.
    fn load(&self, index_name: &str, file_name: &str) -> io::Result<Vec<u8>>;

    /// Save a blob (create or overwrite).
    fn save(&self, index_name: &str, file_name: &str, data: &[u8]) -> io::Result<()>;

    /// Delete a blob. Returns Ok(()) even if the blob didn't exist.
    fn delete(&self, index_name: &str, file_name: &str) -> io::Result<()>;

    /// Check if a blob exists.
    fn exists(&self, index_name: &str, file_name: &str) -> io::Result<bool>;

    /// List all file names for a given index.
    fn list(&self, index_name: &str) -> io::Result<Vec<String>>;
}

/// In-memory blob store for testing.
#[derive(Debug, Clone)]
pub struct MemBlobStore {
    /// `index_name -> file_name -> data`
    data: Arc<RwLock<HashMap<String, HashMap<String, Vec<u8>>>>>,
}

impl MemBlobStore {
    pub fn new() -> Self {
        Self {
            data: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

impl Default for MemBlobStore {
    fn default() -> Self {
        Self::new()
    }
}

impl BlobStore for MemBlobStore {
    fn load(&self, index_name: &str, file_name: &str) -> io::Result<Vec<u8>> {
        let guard = self.data.read().map_err(|_| {
            io::Error::new(io::ErrorKind::Other, "lock poisoned")
        })?;
        guard
            .get(index_name)
            .and_then(|files| files.get(file_name))
            .cloned()
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("{index_name}/{file_name} not found"),
                )
            })
    }

    fn save(&self, index_name: &str, file_name: &str, data: &[u8]) -> io::Result<()> {
        let mut guard = self.data.write().map_err(|_| {
            io::Error::new(io::ErrorKind::Other, "lock poisoned")
        })?;
        guard
            .entry(index_name.to_string())
            .or_default()
            .insert(file_name.to_string(), data.to_vec());
        Ok(())
    }

    fn delete(&self, index_name: &str, file_name: &str) -> io::Result<()> {
        let mut guard = self.data.write().map_err(|_| {
            io::Error::new(io::ErrorKind::Other, "lock poisoned")
        })?;
        if let Some(files) = guard.get_mut(index_name) {
            files.remove(file_name);
        }
        Ok(())
    }

    fn exists(&self, index_name: &str, file_name: &str) -> io::Result<bool> {
        let guard = self.data.read().map_err(|_| {
            io::Error::new(io::ErrorKind::Other, "lock poisoned")
        })?;
        Ok(guard
            .get(index_name)
            .map_or(false, |files| files.contains_key(file_name)))
    }

    fn list(&self, index_name: &str) -> io::Result<Vec<String>> {
        let guard = self.data.read().map_err(|_| {
            io::Error::new(io::ErrorKind::Other, "lock poisoned")
        })?;
        Ok(guard
            .get(index_name)
            .map(|files| files.keys().cloned().collect())
            .unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mem_blob_store_roundtrip() {
        let store = MemBlobStore::new();
        store.save("idx1", "file.bin", b"hello world").unwrap();
        assert!(store.exists("idx1", "file.bin").unwrap());
        assert!(!store.exists("idx1", "other.bin").unwrap());
        let loaded = store.load("idx1", "file.bin").unwrap();
        assert_eq!(loaded, b"hello world");
        store.delete("idx1", "file.bin").unwrap();
        assert!(!store.exists("idx1", "file.bin").unwrap());
    }

    #[test]
    fn test_mem_blob_store_multiple_indexes() {
        let store = MemBlobStore::new();
        store.save("idx1", "a.bin", b"aaa").unwrap();
        store.save("idx2", "a.bin", b"xxx").unwrap();
        assert_eq!(store.load("idx1", "a.bin").unwrap(), b"aaa");
        assert_eq!(store.load("idx2", "a.bin").unwrap(), b"xxx");
    }
}
