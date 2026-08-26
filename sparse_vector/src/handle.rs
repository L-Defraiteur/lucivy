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
use crate::segments::{self, IndexMeta, Segment, SegmentMeta};
use crate::wand::Postings;

const MMAP_FILE: &str = "sparse.mmap";
const VECTORS_FILE: &str = "sparse_vectors.bin";
const DIMS_FILE: &str = "sparse_dims.bin";
/// Legacy bincode file (read-only fallback).
const LEGACY_FILE: &str = "sparse.bin";

/// Files that make up a sparse index (new format).
/// What the single-file layout wrote, and what a commit removes once the
/// index is made of segments. A segmented index's file list is its
/// manifest's (`IndexMeta::files`), which changes at every commit.
const STALE_FILES: &[&str] = &[MMAP_FILE, VECTORS_FILE, DIMS_FILE, LEGACY_FILE];

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
    /// In segmented mode: the vectors inserted since the last commit, and
    /// nothing else. In legacy mode: the whole index, once loaded.
    index: SparseIndex,
    /// Legacy single-file index (`sparse.mmap`, versions 1 to 3 written
    /// before segments). `None` in segmented mode.
    mmap: Option<MmapPostingData>,
    /// True if RAM postings are loaded (always true after create or mutation).
    postings_loaded: bool,
    /// True if vectors HashMap is loaded (always true after create or mutation).
    vectors_loaded: bool,
    /// Cached doc count (valid even when vectors not loaded): the segments'
    /// live vectors plus what is in RAM.
    num_vectors: usize,
    dirty: bool,
    /// The committed segments, oldest first, and the manifest that names
    /// them. Empty in legacy mode.
    segments: Vec<Segment>,
    meta: IndexMeta,
    /// Segments this handle has written, for the next segment's id.
    written: u64,
}

/// An index is segmented when `meta.json` is there. One that is not gets
/// converted by its next commit — the whole-file write it did anyway.
impl Inner {
    fn segmented(&self) -> bool {
        self.mmap.is_none() || !self.meta.segments.is_empty()
    }
}

/// The fields of an index that has nothing yet.
fn empty_inner() -> Inner {
    Inner {
        index: SparseIndex::new(),
        mmap: None,
        postings_loaded: true,
        vectors_loaded: true,
        num_vectors: 0,
        dirty: false,
        segments: Vec::new(),
        meta: IndexMeta::default(),
        written: 0,
    }
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
            inner: Mutex::new(empty_inner()),
            path: PathBuf::from(path),
            backend: StorageBackend::Filesystem,
        };
        handle.commit_inner()?;
        Ok(handle)
    }

    /// Open an existing sparse index.
    /// Tries new mmap format first, falls back to legacy bincode.
    pub fn open(path: &str) -> Result<Self, String> {
        Self::open_backed(Path::new(path), StorageBackend::Filesystem)
    }

    /// Open whatever is in `base`: segments (`meta.json`), the single-file
    /// mmap that came before them, or the bincode that came before that.
    fn open_backed(base: &Path, backend: StorageBackend) -> Result<Self, String> {
        if base.join(segments::META_FILE).exists() {
            Self::open_segmented(base, backend)
        } else if base.join(MMAP_FILE).exists() {
            Self::open_mmap(base, backend)
        } else {
            Self::open_legacy(base)
        }
    }

    /// Open a segmented index: the manifest, then each segment it names.
    /// Nothing is read into RAM — a search walks the mappings.
    fn open_segmented(base: &Path, backend: StorageBackend) -> Result<Self, String> {
        let meta = IndexMeta::read(base)?;
        let mut segments = Vec::with_capacity(meta.segments.len());
        for sm in &meta.segments {
            segments.push(Segment::open(base, sm.clone())?);
        }
        let num_vectors = meta.live_vectors();
        Ok(Self {
            inner: Mutex::new(Inner {
                num_vectors,
                segments,
                meta,
                ..empty_inner()
            }),
            path: base.to_path_buf(),
            backend,
        })
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
            inner: Mutex::new(empty_inner()),
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
            // The local cache of a blob-backed shard is opened like any
            // other index: an interrupted download must not leave half a file.
            mmap_index::write_file_atomic(&cache_dir.join(file_name), &data)?;
        }

        let backend = StorageBackend::Store {
            store,
            index_name: blob_name,
        };

        // Open from cache_dir (same logic as filesystem open)
        if cache_dir.join(segments::META_FILE).exists() || cache_dir.join(MMAP_FILE).exists() {
            Self::open_backed(&cache_dir, backend)
        } else {
            // Empty index (no files in store yet) — create fresh
            let handle = Self {
                inner: Mutex::new(empty_inner()),
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

        // A version 3 file names its own dimensions, in order: the side
        // file is what a dense table needed, and it may not even be there.
        let (dim_map, dim_reverse): (HashMap<u32, usize>, Vec<u32>) = if mmap.has_global_dims() {
            let reverse: Vec<u32> = mmap.tokens().map(|(t, _)| t).collect();
            let map = reverse.iter().enumerate().map(|(i, &t)| (t, i)).collect();
            (map, reverse)
        } else {
            let dims_data = std::fs::read(base.join(DIMS_FILE))
                .map_err(|e| format!("cannot read {DIMS_FILE}: {e}"))?;
            bincode::deserialize(&dims_data)
                .map_err(|e| format!("cannot deserialize dims: {e}"))?
        };

        let num_dims = mmap.num_dims();
        let num_vectors = mmap.num_vectors();
        let empty_postings: Vec<Postings> = (0..num_dims).map(|_| Postings::new()).collect();
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
                ..empty_inner()
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
            inner: Mutex::new(Inner { index, num_vectors, ..empty_inner() }),
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
            // A version 3 file's table is sorted by token id, so a position
            // in the file is not the RAM index's dimension: each list is
            // loaded by its token. Reading by position there loaded every
            // dimension under the wrong one, silently.
            if mmap.has_global_dims() {
                let tokens: Vec<u32> = inner.index.dim_reverse().to_vec();
                let postings = inner.index.postings_mut();
                for (i, pl) in postings.iter_mut().enumerate() {
                    // A dimension the mapping does not name has no postings
                    // here rather than someone else's (a dims side file that
                    // disagrees with the mapping used to index out of bounds).
                    *pl = match tokens.get(i) {
                        Some(&token) => mmap.load_postings_of_token(token),
                        None => Postings::new(),
                    };
                }
            } else {
                let postings = inner.index.postings_mut();
                for (i, pl) in postings.iter_mut().enumerate() {
                    *pl = mmap.load_postings(i);
                }
            }
        }
        inner.postings_loaded = true;
    }

    /// Ensure vectors HashMap is loaded (deserializes from disk if needed).
    fn ensure_vectors_loaded(inner: &mut Inner, path: &Path) -> Result<(), String> {
        if inner.vectors_loaded {
            return Ok(());
        }
        // A segmented index keeps no vectors on disk: `index` holds the
        // delta, which starts empty, and a segment's ids are what tells
        // whether it holds a document (see `segments::Segment::holds`).
        if inner.segmented() {
            inner.vectors_loaded = true;
            return Ok(());
        }
        let vectors_path = path.join(VECTORS_FILE);
        // A single file that names its own dimensions (version 3) does not
        // need this one: it was kept to know which dimensions a deletion
        // touches, and the ids of the segment it converts into are read from
        // its posting lists. A dense file still needs it.
        if !vectors_path.exists()
            && inner.mmap.as_ref().is_some_and(|m| m.has_global_dims())
        {
            inner.vectors_loaded = true;
            return Ok(());
        }
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
        // An update: the copies already committed are hidden, and the one
        // going into RAM will be written into a later segment, which no
        // tombstone covers.
        Self::tombstone_committed(&mut inner, node_id)?;
        inner.index.insert(node_id, vector);
        inner.num_vectors = Self::count(&inner);
        inner.dirty = true;
        Ok(())
    }

    pub fn remove(&self, node_id: u64) -> Result<bool, String> {
        let mut inner = self.inner.lock().map_err(|_| "lock poisoned".to_string())?;
        Self::ensure_vectors_loaded(&mut inner, &self.path)?;
        Self::ensure_postings_loaded(&mut inner);
        let from_segments = Self::tombstone_committed(&mut inner, node_id)?;
        let from_ram = inner.index.remove(node_id);
        let removed = from_segments || from_ram;
        if removed {
            inner.num_vectors = Self::count(&inner);
            inner.dirty = true;
        }
        Ok(removed)
    }

    /// Hide `node_id` in every segment that was written with it. Answers
    /// whether any segment was holding it.
    fn tombstone_committed(inner: &mut Inner, node_id: u64) -> Result<bool, String> {
        let mut hit = false;
        for seg in &mut inner.segments {
            if seg.holds(node_id)? && seg.tombstone(node_id) {
                hit = true;
            }
        }
        if hit {
            // The manifest owns the tombstones; keep it in step with the
            // open segments so a commit writes them out.
            for (sm, seg) in inner.meta.segments.iter_mut().zip(inner.segments.iter()) {
                sm.deleted = seg.meta.deleted.clone();
            }
        }
        Ok(hit)
    }

    /// Live documents: the segments' minus their tombstones, plus RAM.
    fn count(inner: &Inner) -> usize {
        inner.meta.live_vectors() + inner.index.len()
    }

    pub fn search(&self, query: &SparseVector, limit: usize) -> Vec<(u64, f32)> {
        let inner = self.inner.lock().unwrap();
        if !inner.segments.is_empty() {
            return Self::search_segments(&inner, query, limit, &|_| true);
        }
        if !inner.dirty {
            if let Some(ref mmap) = inner.mmap {
                return mmap_index::search_mmap(
                    mmap,
                    inner.index.dim_map(),
                    query,
                    limit,
                    &|_| true,
                );
            }
        }
        inner.index.search(query, limit)
    }

    /// Search every segment, then what is still in RAM, and keep the best
    /// `limit`. A live document sits in exactly one of them — a tombstone
    /// hides the copies a later insert replaced — so the merge has nothing
    /// to deduplicate: it is a sort and a truncation, the same one the
    /// sharded handle does across shards.
    ///
    /// The WAND pruning happens inside each segment rather than over the
    /// whole index; that is the price of segments, and what a merge buys
    /// back.
    fn search_segments<F: Fn(u64) -> bool>(
        inner: &Inner,
        query: &SparseVector,
        limit: usize,
        filter: &F,
    ) -> Vec<(u64, f32)> {
        let mut all: Vec<(u64, f32)> = Vec::new();
        for seg in &inner.segments {
            if seg.data.num_vectors() == 0 {
                continue;
            }
            let keep = |id: u64| seg.is_live(id) && filter(id);
            all.extend(mmap_index::search_mmap(&seg.data, &HashMap::new(), query, limit, &keep));
        }
        if !inner.index.is_empty() {
            all.extend(inner.index.search(query, limit).into_iter().filter(|(id, _)| filter(*id)));
        }
        all.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(&b.0)));
        all.truncate(limit);
        all
    }

    pub fn search_filtered(
        &self,
        query: &SparseVector,
        limit: usize,
        allowed_ids: &[u64],
    ) -> Vec<(u64, f32)> {
        let inner = self.inner.lock().unwrap();
        if !inner.segments.is_empty() {
            let allowed: std::collections::HashSet<u64> = allowed_ids.iter().copied().collect();
            return Self::search_segments(&inner, query, limit, &|id| allowed.contains(&id));
        }
        if !inner.dirty {
            if let Some(ref mmap) = inner.mmap {
                return mmap_index::search_mmap_allowed(
                    mmap,
                    inner.index.dim_map(),
                    query,
                    limit,
                    allowed_ids,
                );
            }
        }
        inner.index.search_filtered(query, limit, allowed_ids)
    }

    /// Merge every segment into one, applying the tombstones — the walk
    /// over sorted token tables described in [`crate::segments`]. What it
    /// buys: one mapping to search instead of N, WAND pruning over the whole
    /// index again, and the deleted documents' bytes back.
    ///
    /// Commits are cheap because they append; this is where that is paid,
    /// once, when the caller decides. Nothing is lost if it is interrupted:
    /// the manifest is only rewritten once the merged segment is on disk.
    pub fn compact(&self) -> Result<(), String> {
        let mut inner = self.inner.lock().map_err(|_| "lock poisoned".to_string())?;
        if inner.dirty {
            drop(inner);
            self.commit_inner()?;
            inner = self.inner.lock().map_err(|_| "lock poisoned".to_string())?;
        }
        if inner.segments.len() < 2 {
            return Ok(());
        }

        inner.written += 1;
        let new_id = segments::new_segment_id(inner.written);
        let sources: Vec<&Segment> = inner.segments.iter().collect();
        let merged = segments::merge_segments(&self.path, &sources, &new_id)?;
        let dropped: Vec<String> = inner.segments.iter()
            .flat_map(|s| [segments::segment_file(&s.meta.id), segments::ids_file(&s.meta.id)])
            .collect();

        // The manifest is what makes the merge real; until it is written,
        // the index is still its old segments.
        inner.meta.segments = vec![merged.clone()];
        inner.meta.write(&self.path)?;
        inner.segments = vec![Segment::open(&self.path, merged)?];
        inner.num_vectors = Self::count(&inner);

        if let StorageBackend::Store { ref store, ref index_name } = self.backend {
            for file in [segments::segment_file(&new_id), segments::ids_file(&new_id), segments::META_FILE.to_string()] {
                let data = std::fs::read(self.path.join(&file))
                    .map_err(|e| format!("cannot read cache {file}: {e}"))?;
                store.save(index_name, &file, &data)
                    .map_err(|e| format!("cannot save {index_name}/{file} to store: {e}"))?;
            }
        }
        // The old segments, now that nothing names them.
        for file in dropped {
            let _ = std::fs::remove_file(self.path.join(&file));
            if let StorageBackend::Store { ref store, ref index_name } = self.backend {
                let _ = store.delete(index_name, &file);
            }
        }
        Ok(())
    }

    /// Segments a commit leaves before merging them, from
    /// `LUCIVY_SPARSE_MAX_SEGMENTS` (`0` never merges on its own).
    ///
    /// Eight by default. Measured (`tests/bench_segment_search.rs`, 100 000
    /// vectors): a search costs the same on 1, 2 or 5 segments, ×1.9 on ten,
    /// ×5.3 on twenty — the WAND pruning works inside a segment, so what is
    /// pruned in one is still walked in the next. Merging every eight
    /// commits keeps a search flat and still leaves seven commits out of
    /// eight paying only for their delta.
    fn max_segments() -> usize {
        static CAP: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
        *CAP.get_or_init(|| {
            std::env::var("LUCIVY_SPARSE_MAX_SEGMENTS").ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(8)
        })
    }

    /// How many segments the index is made of — what a compaction policy
    /// watches, and what a search pays per query dimension.
    pub fn num_segments(&self) -> usize {
        self.inner.lock().map(|i| i.segments.len()).unwrap_or(0)
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().num_vectors
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Write index to disk in the new mmap format, then re-mmap.
    /// If store-backed, also persists to BlobStore.
    /// Write what is in RAM as a **new segment** and point the manifest at
    /// it. What was already committed is not touched: the cost of a commit
    /// is the cost of the delta, where it used to be the cost of the whole
    /// index (`tests/bench_commit_cost.rs`).
    ///
    /// An index written before segments is converted here, once: its whole
    /// content becomes segment zero — the full write it did at every commit
    /// anyway — and `meta.json` appears next to it.
    pub fn commit_inner(&self) -> Result<(), String> {
        let mut inner = self.inner.lock().map_err(|_| "lock poisoned".to_string())?;

        let converting = inner.mmap.is_some();
        if converting {
            // The old file holds everything; bring it into RAM so the
            // segment written below is the whole index, then let it go.
            Self::ensure_postings_loaded(&mut inner);
            Self::ensure_vectors_loaded(&mut inner, &self.path)?;
        } else if !inner.dirty && !inner.meta.segments.is_empty() {
            // Nothing new, and the manifest is already on disk. A tombstone
            // set `dirty`, so this only skips a genuinely idle commit.
            return Ok(());
        }

        // ── The new segment ────────────────────────────────────────────
        let mut written: Vec<String> = Vec::new();
        let has_delta = !inner.index.is_empty()
            || inner.index.postings().iter().any(|p| !p.is_empty());
        if has_delta {
            inner.written += 1;
            let id = segments::new_segment_id(inner.written);
            let file = segments::segment_file(&id);
            mmap_index::write_mmap_file(
                &self.path.join(&file),
                inner.index.postings(),
                inner.index.dim_reverse(),
                inner.index.len() as u32,
            )?;
            // A segment's ids are the ids in its posting lists. In the
            // normal path the RAM index holds them as keys; when converting
            // an older index they are only in the postings, which have just
            // been loaded from its file — the vectors side file may not even
            // exist any more.
            let mut ids: Vec<u64> = if converting {
                let mut set = std::collections::HashSet::new();
                for p in inner.index.postings() {
                    for x in p.as_slice() { set.insert(x.id); }
                }
                set.into_iter().collect()
            } else {
                inner.index.vectors().keys().copied().collect()
            };
            ids.sort_unstable();
            let ids_name = segments::ids_file(&id);
            mmap_index::write_file_atomic(&self.path.join(&ids_name), &segments::encode_ids(&ids))?;
            inner.meta.segments.push(SegmentMeta {
                id,
                num_vectors: ids.len() as u32,
                deleted: Vec::new(),
            });
            written.push(file);
            written.push(ids_name);
        }

        // ── The manifest, last: it is what makes the segment part of the
        // index, and it is written atomically. A crash before this leaves
        // an orphan file and an index that is exactly what it was.
        inner.meta.version = segments::META_VERSION;
        inner.meta.write(&self.path)?;
        written.push(segments::META_FILE.to_string());

        // ── Reopen what was just written, drop the RAM delta ───────────
        let metas: Vec<SegmentMeta> = inner.meta.segments.clone();
        let mut opened = Vec::with_capacity(metas.len());
        for sm in metas {
            opened.push(Segment::open(&self.path, sm)?);
        }
        inner.segments = opened;
        inner.index = SparseIndex::new();
        inner.mmap = None;
        inner.postings_loaded = true;
        inner.vectors_loaded = true;
        inner.dirty = false;
        inner.num_vectors = Self::count(&inner);

        // ── Sync to the store, and drop what the old format left ───────
        if let StorageBackend::Store { ref store, ref index_name } = self.backend {
            for file in &written {
                let data = std::fs::read(self.path.join(file))
                    .map_err(|e| format!("cannot read cache {file}: {e}"))?;
                store
                    .save(index_name, file, &data)
                    .map_err(|e| format!("cannot save {index_name}/{file} to store: {e}"))?;
            }
        }
        // Whatever the old format left — the single mmap and its two side
        // files, or the bincode before them — is not part of a segmented
        // index. Dropped after the manifest names the segments, never before.
        for &stale in STALE_FILES {
            let path = self.path.join(stale);
            if path.exists() {
                let _ = std::fs::remove_file(&path);
                if let StorageBackend::Store { ref store, ref index_name } = self.backend {
                    let _ = store.delete(index_name, stale);
                }
            }
        }

        // Merge when the segments have piled up: cheap commits are paid for
        // here, once every `max_segments()` of them (see `max_segments`).
        let cap = Self::max_segments();
        let pile = inner.segments.len();
        drop(inner);
        if cap > 0 && pile > cap {
            self.compact()?;
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
    fn create_writes_a_manifest_and_a_segment_per_commit() {
        let p = tmp_path("sparse_mmap_create_test");
        cleanup(&p);
        let path = p.to_str().unwrap();

        // An empty index is its manifest, and nothing else: no segment is
        // written for no documents.
        let handle = SparseHandle::create(path).unwrap();
        assert!(p.join(crate::segments::META_FILE).exists());
        assert_eq!(segment_files(&p).len(), 0);
        assert!(!p.join(MMAP_FILE).exists(), "the single-file format is not written any more");

        handle.insert(1, &SparseVector::new(vec![7], vec![1.0])).unwrap();
        handle.commit_inner().unwrap();
        assert_eq!(segment_files(&p).len(), 1);

        // A second commit writes a second segment, not a rewrite of the first.
        handle.insert(2, &SparseVector::new(vec![7], vec![1.0])).unwrap();
        handle.commit_inner().unwrap();
        assert_eq!(segment_files(&p).len(), 2);

        let handle2 = SparseHandle::open(path).unwrap();
        assert_eq!(handle2.len(), 2);
        assert_eq!(handle2.search(&SparseVector::new(vec![7], vec![1.0]), 10).len(), 2);

        cleanup(&p);
    }

    /// The `seg_*.mmap` files of an index directory.
    fn segment_files(base: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(base).unwrap()
            .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
            .filter(|n| n.starts_with("seg_") && n.ends_with(".mmap"))
            .collect();
        names.sort();
        names
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

        // Commit converts it to segments and drops the old files.
        handle.commit_inner().unwrap();
        assert!(p.join(crate::segments::META_FILE).exists());
        assert_eq!(segment_files(&p).len(), 1);
        assert!(!p.join(LEGACY_FILE).exists());
        assert!(!p.join(MMAP_FILE).exists());
        assert_eq!(handle.search(&SparseVector::new(vec![1], vec![1.0]), 10)[0].0, 7);

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

        // The store holds the manifest and the segment's two files.
        let names = store.list("Sparse_test_idx").unwrap();
        assert!(names.iter().any(|n| n == crate::segments::META_FILE), "{names:?}");
        assert_eq!(names.iter().filter(|n| n.ends_with(".mmap")).count(), 1, "{names:?}");
        assert_eq!(names.iter().filter(|n| n.ends_with(".ids")).count(), 1, "{names:?}");

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
        // Docs with token 0: 0 (w=0.0), 10, 20, 30, 40. Doc 0 has weight 0,
        // which is not indexed (see `index`), so it is not a hit.
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
