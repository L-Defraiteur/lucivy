//! Shard storage abstraction.
//!
//! Manages shard directories and root-level files without knowing about
//! concrete index handles (LucivyHandle, SparseHandle, etc.).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::blob_store::BlobStore;

/// Abstraction for shard storage.
///
/// Manages the directory structure for sharded indexes.
/// Each shard gets its own sub-directory/namespace.
/// Root-level files (config, stats) are stored at the base level.
///
/// Concrete handle creation (LucivyHandle, SparseHandle) is left to
/// the consumer — this trait only manages paths and root files.
pub trait ShardStorage: Send + Sync {
    /// Path (or identifier) for a specific shard's directory.
    fn shard_path(&self, shard_id: usize) -> PathBuf;

    /// Write a root-level file (e.g. _shard_config.json).
    fn write_root_file(&self, name: &str, data: &[u8]) -> Result<(), String>;

    /// Read a root-level file.
    fn read_root_file(&self, name: &str) -> Result<Vec<u8>, String>;

    /// Check if a root-level file exists.
    fn root_file_exists(&self, name: &str) -> bool;

    /// Number of shard directories that exist.
    fn count_shards(&self) -> usize {
        let mut count = 0;
        while self.shard_path(count).exists() {
            count += 1;
        }
        count
    }

    /// Ensure a shard directory exists.
    fn ensure_shard_dir(&self, shard_id: usize) -> Result<(), String> {
        let path = self.shard_path(shard_id);
        std::fs::create_dir_all(&path)
            .map_err(|e| format!("cannot create shard_{shard_id} dir: {e}"))
    }
}

/// Filesystem-based shard storage.
pub struct FsShardStorage {
    base_path: PathBuf,
}

impl FsShardStorage {
    /// Create a new FsShardStorage, creating the base directory if needed.
    pub fn new(base_path: impl Into<PathBuf>) -> Result<Self, String> {
        let base_path = base_path.into();
        std::fs::create_dir_all(&base_path)
            .map_err(|e| format!("cannot create base dir: {e}"))?;
        Ok(Self { base_path })
    }

    /// Base path of this storage.
    pub fn base_path(&self) -> &Path {
        &self.base_path
    }
}

impl ShardStorage for FsShardStorage {
    fn shard_path(&self, shard_id: usize) -> PathBuf {
        self.base_path.join(format!("shard_{shard_id}"))
    }

    fn write_root_file(&self, name: &str, data: &[u8]) -> Result<(), String> {
        std::fs::write(self.base_path.join(name), data)
            .map_err(|e| format!("cannot write {name}: {e}"))
    }

    fn read_root_file(&self, name: &str) -> Result<Vec<u8>, String> {
        std::fs::read(self.base_path.join(name))
            .map_err(|e| format!("cannot read {name}: {e}"))
    }

    fn root_file_exists(&self, name: &str) -> bool {
        self.base_path.join(name).exists()
    }
}

/// BlobStore-backed shard storage.
///
/// Each shard gets a namespace `{index_name}/shard_{id}` in the blob store.
/// Root-level files use `{index_name}` directly.
pub struct BlobShardStorage<S: BlobStore> {
    store: Arc<S>,
    index_name: String,
    /// Local cache base for materialized files (optional, for mmap).
    cache_base: Option<PathBuf>,
}

impl<S: BlobStore> BlobShardStorage<S> {
    pub fn new(store: Arc<S>, index_name: impl Into<String>, cache_base: Option<PathBuf>) -> Self {
        Self {
            store,
            index_name: index_name.into(),
            cache_base,
        }
    }

    /// BlobStore namespace for a shard.
    pub fn shard_namespace(&self, shard_id: usize) -> String {
        format!("{}/shard_{shard_id}", self.index_name)
    }

    /// Access the underlying store.
    pub fn store(&self) -> &S {
        &self.store
    }
}

impl<S: BlobStore> ShardStorage for BlobShardStorage<S> {
    fn shard_path(&self, shard_id: usize) -> PathBuf {
        // For BlobStore, the "path" is the local cache directory.
        match &self.cache_base {
            Some(base) => base.join(format!("shard_{shard_id}")),
            None => PathBuf::from(format!("/tmp/lucistore_cache/shard_{shard_id}")),
        }
    }

    fn write_root_file(&self, name: &str, data: &[u8]) -> Result<(), String> {
        self.store.save(&self.index_name, name, data)
            .map_err(|e| format!("cannot write {name} to blob store: {e}"))
    }

    fn read_root_file(&self, name: &str) -> Result<Vec<u8>, String> {
        self.store.load(&self.index_name, name)
            .map_err(|e| format!("cannot read {name} from blob store: {e}"))
    }

    fn root_file_exists(&self, name: &str) -> bool {
        self.store.exists(&self.index_name, name).unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fs_shard_storage() {
        let tmp = std::env::temp_dir().join("lucistore_test_fs_shard");
        let _ = std::fs::remove_dir_all(&tmp);

        let storage = FsShardStorage::new(&tmp).unwrap();
        assert_eq!(storage.shard_path(0), tmp.join("shard_0"));
        assert_eq!(storage.shard_path(3), tmp.join("shard_3"));

        // Root files.
        storage.write_root_file("config.json", b"{\"shards\":4}").unwrap();
        assert!(storage.root_file_exists("config.json"));
        let data = storage.read_root_file("config.json").unwrap();
        assert_eq!(data, b"{\"shards\":4}");

        // Shard dir creation.
        storage.ensure_shard_dir(0).unwrap();
        assert!(storage.shard_path(0).exists());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_blob_shard_storage() {
        use crate::blob_store::MemBlobStore;

        let store = Arc::new(MemBlobStore::new());
        let storage = BlobShardStorage::new(store.clone(), "test_idx", None);

        storage.write_root_file("config.json", b"{}").unwrap();
        assert!(storage.root_file_exists("config.json"));
        assert_eq!(storage.read_root_file("config.json").unwrap(), b"{}");

        assert_eq!(storage.shard_namespace(2), "test_idx/shard_2");
    }
}
