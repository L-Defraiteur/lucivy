//! Sparse vector index handle with mmap persistence.
//!
//! Commit writes a flat binary mmap format (sparse.mmap) + bincode side files.
//! Open mmap's the posting data (O(1)), vectors + dims loaded lazily.
//! Search uses mmap iterators when available (no RAM postings or vectors needed).
//! Mutations load postings + vectors into RAM on first access, set dirty flag.
//!
//! Two storage modes:
//! - **Filesystem** (`create`/`open`): files live directly in the given directory.
//! - **BlobStore** (`create_with_store`/`open_with_store`): source of truth is the
//!   BlobStore; a local tmpdir is used as mmap cache. Cleaned up on Drop.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::blob_store::BlobStore;
use crate::index::{SparseIndex, SparseVector};
use crate::mmap_index::{self, MmapPostingData};
use crate::posting_list::PostingList;

const MMAP_FILE: &str = "sparse.mmap";
const VECTORS_FILE: &str = "sparse_vectors.bin";
const DIMS_FILE: &str = "sparse_dims.bin";
/// Legacy bincode file (read-only fallback).
const LEGACY_FILE: &str = "sparse.bin";

/// Files that make up a sparse index (new format).
const INDEX_FILES: &[&str] = &[MMAP_FILE, VECTORS_FILE, DIMS_FILE];

/// BlobStore key prefix — ensures no collision with other index types (FTS, etc.)
const BLOB_PREFIX: &str = "Sparse_";

/// Monotonic counter for unique tmpdir names.
static CACHE_SEQ: AtomicUsize = AtomicUsize::new(0);

/// Storage backend for persistence.
enum StorageBackend {
    /// Files live directly in `path`. No external store.
    Filesystem,
    /// Source of truth is a BlobStore. `path` is a local tmpdir cache for mmap.
    Store {
        store: Arc<dyn BlobStore>,
        index_name: String,
    },
}

struct Inner {
    index: SparseIndex,
    mmap: Option<MmapPostingData>,
    /// True if RAM postings are loaded (always true after create or mutation).
    postings_loaded: bool,
    /// True if vectors HashMap is loaded (always true after create or mutation).
    vectors_loaded: bool,
    /// Cached doc count (valid even when vectors not loaded).
    num_vectors: usize,
    dirty: bool,
}

pub struct SparseHandle {
    inner: Mutex<Inner>,
    path: PathBuf,
    backend: StorageBackend,
}

impl SparseHandle {
    // -----------------------------------------------------------------------
    // Filesystem lifecycle (existing API, unchanged behavior)
    // -----------------------------------------------------------------------

    /// Create a new empty sparse index at the given path.
    pub fn create(path: &str) -> Result<Self, String> {
        std::fs::create_dir_all(Path::new(path))
            .map_err(|e| format!("cannot create directory {path}: {e}"))?;
        let handle = Self {
            inner: Mutex::new(Inner {
                index: SparseIndex::new(),
                mmap: None,
                postings_loaded: true,
                vectors_loaded: true,
                num_vectors: 0,
                dirty: false,
            }),
            path: PathBuf::from(path),
            backend: StorageBackend::Filesystem,
        };
        handle.commit_inner()?;
        Ok(handle)
    }

    /// Open an existing sparse index.
    /// Tries new mmap format first, falls back to legacy bincode.
    pub fn open(path: &str) -> Result<Self, String> {
        let base = Path::new(path);
        let mmap_path = base.join(MMAP_FILE);

        if mmap_path.exists() {
            Self::open_mmap(base, StorageBackend::Filesystem)
        } else {
            Self::open_legacy(base)
        }
    }

    // -----------------------------------------------------------------------
    // BlobStore lifecycle
    // -----------------------------------------------------------------------

    /// Create a new empty sparse index backed by a BlobStore.
    ///
    /// `cache_base` is the root directory for mmap caches. Inside it, a unique
    /// subdirectory `{pid}/{index_name}_{seq}` is created automatically.
    /// Source of truth is the store.
    pub fn create_with_store(
        store: Arc<dyn BlobStore>,
        index_name: &str,
        cache_base: &Path,
    ) -> Result<Self, String> {
        let blob_name = format!("{BLOB_PREFIX}{index_name}");
        let cache_dir = Self::make_cache_dir(cache_base, &blob_name)?;

        let handle = Self {
            inner: Mutex::new(Inner {
                index: SparseIndex::new(),
                mmap: None,
                postings_loaded: true,
                vectors_loaded: true,
                num_vectors: 0,
                dirty: false,
            }),
            path: cache_dir,
            backend: StorageBackend::Store {
                store,
                index_name: blob_name,
            },
        };
        handle.commit_inner()?;
        Ok(handle)
    }

    /// Open an existing sparse index from a BlobStore.
    ///
    /// `cache_base` is the root directory for mmap caches. Blobs are materialized
    /// from the store into `{cache_base}/{pid}/{index_name}_{seq}/`, then mmap'd.
    pub fn open_with_store(
        store: Arc<dyn BlobStore>,
        index_name: &str,
        cache_base: &Path,
    ) -> Result<Self, String> {
        let blob_name = format!("{BLOB_PREFIX}{index_name}");
        let cache_dir = Self::make_cache_dir(cache_base, &blob_name)?;

        // Materialize all blobs from store to cache_dir
        let files = store
            .list(&blob_name)
            .map_err(|e| format!("cannot list blobs for {blob_name}: {e}"))?;

        for file_name in &files {
            let data = store
                .load(&blob_name, file_name)
                .map_err(|e| format!("cannot load {blob_name}/{file_name}: {e}"))?;
            std::fs::write(cache_dir.join(file_name), data)
                .map_err(|e| format!("cannot write cache {file_name}: {e}"))?;
        }

        let backend = StorageBackend::Store {
            store,
            index_name: blob_name,
        };

        // Open from cache_dir (same logic as filesystem open)
        if cache_dir.join(MMAP_FILE).exists() {
            Self::open_mmap(&cache_dir, backend)
        } else {
            // Empty index (no files in store yet) — create fresh
            let handle = Self {
                inner: Mutex::new(Inner {
                    index: SparseIndex::new(),
                    mmap: None,
                    postings_loaded: true,
                    vectors_loaded: true,
                    num_vectors: 0,
                    dirty: false,
                }),
                path: cache_dir,
                backend,
            };
            handle.commit_inner()?;
            Ok(handle)
        }
    }

    /// Create a unique cache directory for BlobStore mmap files.
    ///
    /// Layout: `{base}/{pid}/{index_name}_{seq}/`
    /// - PID isolates between processes
    /// - Atomic seq isolates between threads / multiple opens
    fn make_cache_dir(base: &Path, index_name: &str) -> Result<PathBuf, String> {
        let seq = CACHE_SEQ.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let dir = base
            .join(format!("{pid}"))
            .join(format!("{index_name}_{seq}"));
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("cannot create cache dir {}: {e}", dir.display()))?;
        Ok(dir)
    }

    // -----------------------------------------------------------------------
    // Shared open helpers
    // -----------------------------------------------------------------------

    /// Open using the new mmap format.
    /// Only mmap + dims are loaded. Postings and vectors are lazy.
    fn open_mmap(base: &Path, backend: StorageBackend) -> Result<Self, String> {
        let mmap = MmapPostingData::open(&base.join(MMAP_FILE))?;

        // Deserialize dimension mapping (small file, needed for search routing)
        let dims_data = std::fs::read(base.join(DIMS_FILE))
            .map_err(|e| format!("cannot read {DIMS_FILE}: {e}"))?;
        let (dim_map, dim_reverse): (HashMap<u32, usize>, Vec<u32>) =
            bincode::deserialize(&dims_data)
                .map_err(|e| format!("cannot deserialize dims: {e}"))?;

        let num_dims = mmap.num_dims();
        let num_vectors = mmap.num_vectors();
        let empty_postings: Vec<PostingList> =
            (0..num_dims).map(|_| PostingList::default()).collect();
        // Empty vectors — will be loaded lazily on first mutation
        let index =
            SparseIndex::from_parts(dim_map, dim_reverse, empty_postings, HashMap::new());

        Ok(Self {
            inner: Mutex::new(Inner {
                index,
                mmap: Some(mmap),
                postings_loaded: false,
                vectors_loaded: false,
                num_vectors,
                dirty: false,
            }),
            path: base.to_path_buf(),
            backend,
        })
    }

    /// Open using legacy bincode format (sparse.bin).
    fn open_legacy(base: &Path) -> Result<Self, String> {
        let data_path = base.join(LEGACY_FILE);
        let data = std::fs::read(&data_path)
            .map_err(|e| format!("cannot read {}: {e}", data_path.display()))?;
        let index: SparseIndex = bincode::deserialize(&data)
            .map_err(|e| format!("cannot deserialize sparse index: {e}"))?;
        let num_vectors = index.len();
        Ok(Self {
            inner: Mutex::new(Inner {
                index,
                mmap: None,
                postings_loaded: true,
                vectors_loaded: true,
                num_vectors,
                dirty: false,
            }),
            path: base.to_path_buf(),
            backend: StorageBackend::Filesystem,
        })
    }

    // -----------------------------------------------------------------------
    // Lazy loading
    // -----------------------------------------------------------------------

    /// Ensure RAM postings are loaded (materializes from mmap if needed).
    fn ensure_postings_loaded(inner: &mut Inner) {
        if inner.postings_loaded {
            return;
        }
        if let Some(ref mmap) = inner.mmap {
            let postings = inner.index.postings_mut();
            for (i, pl) in postings.iter_mut().enumerate() {
                *pl = mmap.load_posting_list(i);
            }
        }
        inner.postings_loaded = true;
    }

    /// Ensure vectors HashMap is loaded (deserializes from disk if needed).
    fn ensure_vectors_loaded(inner: &mut Inner, path: &Path) -> Result<(), String> {
        if inner.vectors_loaded {
            return Ok(());
        }
        let vectors_path = path.join(VECTORS_FILE);
        let data = std::fs::read(&vectors_path)
            .map_err(|e| format!("cannot read {}: {e}", vectors_path.display()))?;
        let vectors: HashMap<u64, SparseVector> = bincode::deserialize(&data)
            .map_err(|e| format!("cannot deserialize vectors: {e}"))?;
        inner.index.set_vectors(vectors);
        inner.vectors_loaded = true;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Public API (called from bridge)
    // -----------------------------------------------------------------------

    pub fn insert(&self, node_id: u64, vector: &SparseVector) -> Result<(), String> {
        let mut inner = self.inner.lock().map_err(|_| "lock poisoned".to_string())?;
        Self::ensure_vectors_loaded(&mut inner, &self.path)?;
        Self::ensure_postings_loaded(&mut inner);
        inner.index.insert(node_id, vector);
        inner.num_vectors = inner.index.len();
        inner.dirty = true;
        Ok(())
    }

    pub fn remove(&self, node_id: u64) -> Result<bool, String> {
        let mut inner = self.inner.lock().map_err(|_| "lock poisoned".to_string())?;
        Self::ensure_vectors_loaded(&mut inner, &self.path)?;
        Self::ensure_postings_loaded(&mut inner);
        let removed = inner.index.remove(node_id);
        if removed {
            inner.num_vectors = inner.index.len();
            inner.dirty = true;
        }
        Ok(removed)
    }

    pub fn search(&self, query: &SparseVector, limit: usize) -> Vec<(u64, f32)> {
        let inner = self.inner.lock().unwrap();
        if !inner.dirty {
            if let Some(ref mmap) = inner.mmap {
                return mmap_index::search_mmap(
                    mmap,
                    inner.index.dim_map(),
                    inner.index.pool(),
                    query,
                    limit,
                    &|_| true,
                );
            }
        }
        inner.index.search(query, limit)
    }

    pub fn search_filtered(
        &self,
        query: &SparseVector,
        limit: usize,
        allowed_ids: &[u64],
    ) -> Vec<(u64, f32)> {
        let inner = self.inner.lock().unwrap();
        if !inner.dirty {
            if let Some(ref mmap) = inner.mmap {
                if allowed_ids.is_empty() {
                    return Vec::new();
                }
                let allowed: std::collections::HashSet<u64> =
                    allowed_ids.iter().copied().collect();
                return mmap_index::search_mmap(
                    mmap,
                    inner.index.dim_map(),
                    inner.index.pool(),
                    query,
                    limit,
                    &|id| allowed.contains(&id),
                );
            }
        }
        inner.index.search_filtered(query, limit, allowed_ids)
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().num_vectors
    }

    /// Write index to disk in the new mmap format, then re-mmap.
    /// If store-backed, also persists to BlobStore.
    pub fn commit_inner(&self) -> Result<(), String> {
        let mut inner = self.inner.lock().map_err(|_| "lock poisoned".to_string())?;

        // If postings not loaded and not dirty, nothing to write (already on disk)
        // But we still need to write on create (postings_loaded=true, dirty=false, mmap=None)
        if !inner.postings_loaded && !inner.dirty && inner.mmap.is_some() {
            return Ok(());
        }

        Self::ensure_postings_loaded(&mut inner);
        Self::ensure_vectors_loaded(&mut inner, &self.path)?;

        // Write sparse.mmap to local cache
        mmap_index::write_mmap_file(
            &self.path.join(MMAP_FILE),
            inner.index.postings(),
            inner.num_vectors as u32,
        )?;

        // Serialize vectors + dims
        let vectors_data = bincode::serialize(inner.index.vectors())
            .map_err(|e| format!("cannot serialize vectors: {e}"))?;
        std::fs::write(self.path.join(VECTORS_FILE), &vectors_data)
            .map_err(|e| format!("cannot write {VECTORS_FILE}: {e}"))?;

        let dims_data =
            bincode::serialize(&(inner.index.dim_map(), inner.index.dim_reverse()))
                .map_err(|e| format!("cannot serialize dims: {e}"))?;
        std::fs::write(self.path.join(DIMS_FILE), &dims_data)
            .map_err(|e| format!("cannot write {DIMS_FILE}: {e}"))?;

        // Sync to BlobStore if store-backed
        if let StorageBackend::Store { ref store, ref index_name } = self.backend {
            for &file in INDEX_FILES {
                let data = std::fs::read(self.path.join(file))
                    .map_err(|e| format!("cannot read cache {file}: {e}"))?;
                store
                    .save(index_name, file, &data)
                    .map_err(|e| format!("cannot save {index_name}/{file} to store: {e}"))?;
            }
        }

        // Re-mmap and reset to lazy state
        let mmap = MmapPostingData::open(&self.path.join(MMAP_FILE))?;
        inner.mmap = Some(mmap);
        inner.dirty = false;
        // Note: postings_loaded and vectors_loaded stay true — data is still in RAM.
        // They'll be reset on next open().

        // Remove legacy file if present
        let legacy = self.path.join(LEGACY_FILE);
        if legacy.exists() {
            let _ = std::fs::remove_file(&legacy);
            // Also remove from store if store-backed
            if let StorageBackend::Store { ref store, ref index_name } = self.backend {
                let _ = store.delete(index_name, LEGACY_FILE);
            }
        }

        Ok(())
    }
}

impl Drop for SparseHandle {
    fn drop(&mut self) {
        // Only clean up cache_dir for store-backed handles (tmpdir we created).
        // Filesystem handles use the user's data directory — never delete it.
        if let StorageBackend::Store { .. } = &self.backend {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blob_store::MemBlobStore;

    fn tmp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(name)
    }

    fn cleanup(path: &Path) {
        let _ = std::fs::remove_dir_all(path);
    }

    // -----------------------------------------------------------------------
    // Filesystem tests (unchanged)
    // -----------------------------------------------------------------------

    #[test]
    fn create_writes_mmap_format() {
        let p = tmp_path("sparse_mmap_create_test");
        cleanup(&p);
        let path = p.to_str().unwrap();

        let _handle = SparseHandle::create(path).unwrap();
        assert!(p.join(MMAP_FILE).exists());
        assert!(p.join(VECTORS_FILE).exists());
        assert!(p.join(DIMS_FILE).exists());

        let handle2 = SparseHandle::open(path).unwrap();
        assert_eq!(handle2.len(), 0);

        cleanup(&p);
    }

    #[test]
    fn persistence_roundtrip_mmap() {
        let p = tmp_path("sparse_mmap_roundtrip_test");
        cleanup(&p);
        let path = p.to_str().unwrap();

        let handle = SparseHandle::create(path).unwrap();
        handle
            .insert(42, &SparseVector::new(vec![1, 2], vec![0.5, 0.3]))
            .unwrap();
        handle
            .insert(99, &SparseVector::new(vec![2, 3], vec![0.8, 0.2]))
            .unwrap();
        handle.commit_inner().unwrap();
        drop(handle);

        // Reopen — should use mmap path
        let handle2 = SparseHandle::open(path).unwrap();
        assert_eq!(handle2.len(), 2);

        // Search via mmap (no RAM postings loaded)
        let results = handle2.search(&SparseVector::new(vec![2], vec![1.0]), 10);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, 99);
        assert!((results[0].1 - 0.8).abs() < 1e-6);
        assert_eq!(results[1].0, 42);
        assert!((results[1].1 - 0.3).abs() < 1e-6);

        cleanup(&p);
    }

    #[test]
    fn mmap_search_filtered() {
        let p = tmp_path("sparse_mmap_filtered_test");
        cleanup(&p);
        let path = p.to_str().unwrap();

        let handle = SparseHandle::create(path).unwrap();
        handle
            .insert(1, &SparseVector::new(vec![1, 2], vec![0.5, 0.3]))
            .unwrap();
        handle
            .insert(2, &SparseVector::new(vec![1, 3], vec![0.9, 0.1]))
            .unwrap();
        handle
            .insert(3, &SparseVector::new(vec![1], vec![0.7]))
            .unwrap();
        handle.commit_inner().unwrap();
        drop(handle);

        let handle2 = SparseHandle::open(path).unwrap();
        let results = handle2.search_filtered(&SparseVector::new(vec![1], vec![1.0]), 10, &[1, 3]);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, 3); // 0.7
        assert_eq!(results[1].0, 1); // 0.5

        cleanup(&p);
    }

    #[test]
    fn mutation_after_mmap_open() {
        let p = tmp_path("sparse_mmap_mutation_test");
        cleanup(&p);
        let path = p.to_str().unwrap();

        let handle = SparseHandle::create(path).unwrap();
        handle
            .insert(1, &SparseVector::new(vec![10], vec![1.0]))
            .unwrap();
        handle.commit_inner().unwrap();
        drop(handle);

        // Reopen, mutate (triggers postings load from mmap), search
        let handle2 = SparseHandle::open(path).unwrap();
        handle2
            .insert(2, &SparseVector::new(vec![10], vec![2.0]))
            .unwrap();

        let results = handle2.search(&SparseVector::new(vec![10], vec![1.0]), 10);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, 2); // 2.0
        assert_eq!(results[1].0, 1); // 1.0

        // Commit and reopen again
        handle2.commit_inner().unwrap();
        drop(handle2);

        let handle3 = SparseHandle::open(path).unwrap();
        let results = handle3.search(&SparseVector::new(vec![10], vec![1.0]), 10);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, 2);

        cleanup(&p);
    }

    #[test]
    fn legacy_fallback() {
        let p = tmp_path("sparse_mmap_legacy_test");
        cleanup(&p);
        let path = p.to_str().unwrap();

        // Write legacy format manually
        std::fs::create_dir_all(&p).unwrap();
        let mut index = SparseIndex::new();
        index.insert(7, &SparseVector::new(vec![1], vec![0.42]));
        let data = bincode::serialize(&index).unwrap();
        std::fs::write(p.join(LEGACY_FILE), data).unwrap();

        // Open should fall back to legacy
        let handle = SparseHandle::open(path).unwrap();
        assert_eq!(handle.len(), 1);
        let results = handle.search(&SparseVector::new(vec![1], vec![1.0]), 10);
        assert_eq!(results[0].0, 7);

        // Commit migrates to new format
        handle.commit_inner().unwrap();
        assert!(p.join(MMAP_FILE).exists());
        assert!(!p.join(LEGACY_FILE).exists());

        cleanup(&p);
    }

    #[test]
    fn many_docs_mmap_roundtrip() {
        let p = tmp_path("sparse_mmap_many_docs_test");
        cleanup(&p);
        let path = p.to_str().unwrap();

        let handle = SparseHandle::create(path).unwrap();
        for i in 0..500u64 {
            let token = (i % 50) as u32;
            let weight = (i as f32) / 500.0;
            handle
                .insert(
                    i,
                    &SparseVector::new(vec![token, token + 50], vec![weight, weight * 0.5]),
                )
                .unwrap();
        }
        handle.commit_inner().unwrap();
        drop(handle);

        let handle2 = SparseHandle::open(path).unwrap();
        assert_eq!(handle2.len(), 500);

        let results = handle2.search(&SparseVector::new(vec![0, 50], vec![1.0, 1.0]), 5);
        assert_eq!(results.len(), 5);
        // Doc 450 has weight 0.9 for token 0, 0.45 for token 50 → score 1.35
        assert_eq!(results[0].0, 450);

        cleanup(&p);
    }

    // -----------------------------------------------------------------------
    // BlobStore tests
    // -----------------------------------------------------------------------

    fn test_cache_base() -> PathBuf {
        std::env::temp_dir().join("sparse_test_cache")
    }

    #[test]
    fn blob_store_create_and_search() {
        let store = Arc::new(MemBlobStore::new());
        let cb = test_cache_base();
        let handle = SparseHandle::create_with_store(store.clone(), "test_idx", &cb).unwrap();

        handle
            .insert(42, &SparseVector::new(vec![1, 2], vec![0.5, 0.3]))
            .unwrap();
        handle
            .insert(99, &SparseVector::new(vec![2, 3], vec![0.8, 0.2]))
            .unwrap();
        handle.commit_inner().unwrap();

        // Data should be in store
        assert!(store.exists("Sparse_test_idx", MMAP_FILE).unwrap());
        assert!(store.exists("Sparse_test_idx", VECTORS_FILE).unwrap());
        assert!(store.exists("Sparse_test_idx", DIMS_FILE).unwrap());
        assert_eq!(store.list("Sparse_test_idx").unwrap().len(), 3);

        // Search should work (mmap from cache)
        let results = handle.search(&SparseVector::new(vec![2], vec![1.0]), 10);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, 99);
    }

    #[test]
    fn blob_store_close_reopen() {
        let store = Arc::new(MemBlobStore::new());
        let cb = test_cache_base();

        // Create, insert, commit, drop
        {
            let handle = SparseHandle::create_with_store(store.clone(), "reopen_idx", &cb).unwrap();
            handle
                .insert(1, &SparseVector::new(vec![10], vec![1.0]))
                .unwrap();
            handle
                .insert(2, &SparseVector::new(vec![10, 20], vec![0.5, 0.8]))
                .unwrap();
            handle.commit_inner().unwrap();
        }
        // Handle dropped → cache_dir cleaned up

        // Reopen from store
        let handle2 = SparseHandle::open_with_store(store.clone(), "reopen_idx", &cb).unwrap();
        assert_eq!(handle2.len(), 2);

        let results = handle2.search(&SparseVector::new(vec![10], vec![1.0]), 10);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, 1); // 1.0
        assert_eq!(results[1].0, 2); // 0.5
    }

    #[test]
    fn blob_store_mutation_after_reopen() {
        let store = Arc::new(MemBlobStore::new());
        let cb = test_cache_base();

        {
            let handle = SparseHandle::create_with_store(store.clone(), "mut_idx", &cb).unwrap();
            handle
                .insert(1, &SparseVector::new(vec![5], vec![1.0]))
                .unwrap();
            handle.commit_inner().unwrap();
        }

        let handle2 = SparseHandle::open_with_store(store.clone(), "mut_idx", &cb).unwrap();
        handle2
            .insert(2, &SparseVector::new(vec![5], vec![2.0]))
            .unwrap();
        handle2.commit_inner().unwrap();

        // Reopen again — should have both docs
        drop(handle2);
        let handle3 = SparseHandle::open_with_store(store.clone(), "mut_idx", &cb).unwrap();
        assert_eq!(handle3.len(), 2);

        let results = handle3.search(&SparseVector::new(vec![5], vec![1.0]), 10);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, 2); // 2.0
        assert_eq!(results[1].0, 1); // 1.0
    }

    #[test]
    fn blob_store_delete_and_reopen() {
        let store = Arc::new(MemBlobStore::new());
        let cb = test_cache_base();

        {
            let handle = SparseHandle::create_with_store(store.clone(), "del_idx", &cb).unwrap();
            handle
                .insert(1, &SparseVector::new(vec![1], vec![1.0]))
                .unwrap();
            handle
                .insert(2, &SparseVector::new(vec![1], vec![2.0]))
                .unwrap();
            handle.commit_inner().unwrap();
        }

        // Reopen, delete, commit
        let handle2 = SparseHandle::open_with_store(store.clone(), "del_idx", &cb).unwrap();
        assert_eq!(handle2.len(), 2);
        handle2.remove(1).unwrap();
        assert_eq!(handle2.len(), 1);
        handle2.commit_inner().unwrap();
        drop(handle2);

        // Reopen — should have 1 doc
        let handle3 = SparseHandle::open_with_store(store.clone(), "del_idx", &cb).unwrap();
        assert_eq!(handle3.len(), 1);

        let results = handle3.search(&SparseVector::new(vec![1], vec![1.0]), 10);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, 2);
    }

    #[test]
    fn blob_store_multiple_indexes_isolated() {
        let store = Arc::new(MemBlobStore::new());
        let cb = test_cache_base();

        let h1 = SparseHandle::create_with_store(store.clone(), "idx_a", &cb).unwrap();
        let h2 = SparseHandle::create_with_store(store.clone(), "idx_b", &cb).unwrap();

        h1.insert(1, &SparseVector::new(vec![1], vec![1.0]))
            .unwrap();
        h1.insert(2, &SparseVector::new(vec![1], vec![0.5]))
            .unwrap();
        h2.insert(10, &SparseVector::new(vec![1], vec![3.0]))
            .unwrap();

        h1.commit_inner().unwrap();
        h2.commit_inner().unwrap();

        assert_eq!(h1.len(), 2);
        assert_eq!(h2.len(), 1);

        // Store has separate blobs
        assert_eq!(store.list("Sparse_idx_a").unwrap().len(), 3);
        assert_eq!(store.list("Sparse_idx_b").unwrap().len(), 3);
    }

    #[test]
    fn blob_store_survives_cache_cleanup() {
        let store = Arc::new(MemBlobStore::new());
        let cb = test_cache_base();

        {
            let handle = SparseHandle::create_with_store(store.clone(), "surv_idx", &cb).unwrap();
            for i in 0..50u64 {
                handle
                    .insert(i, &SparseVector::new(vec![(i % 10) as u32], vec![i as f32]))
                    .unwrap();
            }
            handle.commit_inner().unwrap();
        }
        // Cache cleaned up on drop

        // Reopen from store
        let handle2 = SparseHandle::open_with_store(store.clone(), "surv_idx", &cb).unwrap();
        assert_eq!(handle2.len(), 50);

        let results = handle2.search(&SparseVector::new(vec![0], vec![1.0]), 5);
        // Docs with token 0: 0 (w=0.0), 10, 20, 30, 40. Doc 0 has weight 0 → no score.
        assert_eq!(results.len(), 4);
        assert_eq!(results[0].0, 40);
    }

    #[test]
    fn blob_store_search_filtered_after_reopen() {
        let store = Arc::new(MemBlobStore::new());
        let cb = test_cache_base();

        {
            let handle = SparseHandle::create_with_store(store.clone(), "filt_idx", &cb).unwrap();
            handle
                .insert(1, &SparseVector::new(vec![1, 2], vec![0.5, 0.3]))
                .unwrap();
            handle
                .insert(2, &SparseVector::new(vec![1, 3], vec![0.9, 0.1]))
                .unwrap();
            handle
                .insert(3, &SparseVector::new(vec![1], vec![0.7]))
                .unwrap();
            handle.commit_inner().unwrap();
        }

        let handle2 = SparseHandle::open_with_store(store.clone(), "filt_idx", &cb).unwrap();
        let results =
            handle2.search_filtered(&SparseVector::new(vec![1], vec![1.0]), 10, &[1, 3]);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, 3); // 0.7
        assert_eq!(results[1].0, 1); // 0.5
    }
}
