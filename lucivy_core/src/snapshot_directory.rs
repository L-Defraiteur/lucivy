//! A read-only `Directory` over a LUCE snapshot held in memory.
//!
//! `import_snapshot` extracts a snapshot into files: the blob and the files
//! exist at once, so opening a 2.3 GB index costs 4.6 GB — more than the 4 GB
//! WebAssembly can address, on exactly the index one wanted to serve. This
//! directory keeps the blob and hands out `FileSlice`s that point into it. One
//! allocation, its size known before it is made (the blob's own length), and
//! no duplication.
//!
//! It also removes the per-file cost that dominates browser reads: one open
//! instead of one per file — measured at about 3 ms each on OPFS, roughly 900
//! files for a four-shard index.
//!
//! Writing is refused. An index served this way answers queries; indexing
//! writes to a real directory, packages a snapshot, and this reads it back.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use ld_lucivy::directory::error::{DeleteError, OpenReadError, OpenWriteError};
use ld_lucivy::directory::{
    Directory, FileHandle, FileSlice, WatchCallback, WatchCallbackList, WatchHandle, WritePtr,
};
use ld_lucivy::directory::OwnedBytes;

/// The files of one index inside a snapshot, as slices of the blob.
#[derive(Clone)]
pub struct SnapshotDirectory {
    blob: OwnedBytes,
    /// Relative path → byte range in the blob.
    files: Arc<HashMap<PathBuf, (usize, usize)>>,
    /// Never fires: nothing writes here. Kept because `Directory` requires it
    /// and a reader still registers watches.
    watch_router: Arc<RwLock<WatchCallbackList>>,
}

impl std::fmt::Debug for SnapshotDirectory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SnapshotDirectory({} files, {} bytes)", self.files.len(), self.blob.len())
    }
}

impl SnapshotDirectory {
    /// The directories of every index in `blob`, keyed by the snapshot's index
    /// path (`shard_0`, …), plus the snapshot's root files.
    ///
    /// The blob is shared by every directory: `OwnedBytes` is an `Arc` over the
    /// bytes, so N shards cost one copy, not N.
    pub fn open_all(
        blob: OwnedBytes,
    ) -> Result<(Vec<(String, SnapshotDirectory)>, Vec<(String, OwnedBytes)>), String> {
        let manifest = lucistore::snapshot::read_manifest(blob.as_slice())?;
        let root: Vec<(String, OwnedBytes)> = manifest
            .root_files
            .iter()
            .map(|e| (e.name.clone(), blob.slice(e.offset..e.offset + e.len)))
            .collect();
        let mut out = Vec::with_capacity(manifest.indexes.len());
        for (path, entries) in &manifest.indexes {
            let files: HashMap<PathBuf, (usize, usize)> = entries
                .iter()
                .map(|e| (PathBuf::from(&e.name), (e.offset, e.len)))
                .collect();
            out.push((
                path.clone(),
                SnapshotDirectory {
                    blob: blob.clone(),
                    files: Arc::new(files),
                    watch_router: Arc::new(RwLock::new(WatchCallbackList::default())),
                },
            ));
        }
        Ok((out, root))
    }

    /// Bytes the blob holds — what an index served this way costs in memory.
    pub fn blob_len(&self) -> usize {
        self.blob.len()
    }

    pub fn num_files(&self) -> usize {
        self.files.len()
    }

    fn slice_of(&self, path: &Path) -> Result<OwnedBytes, OpenReadError> {
        let (offset, len) = *self
            .files
            .get(path)
            .ok_or_else(|| OpenReadError::FileDoesNotExist(path.to_path_buf()))?;
        Ok(self.blob.slice(offset..offset + len))
    }

    fn read_only(path: &Path) -> io::Error {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{}: a snapshot directory is read-only", path.display()),
        )
    }
}

impl Directory for SnapshotDirectory {
    fn get_file_handle(&self, path: &Path) -> Result<Arc<dyn FileHandle>, OpenReadError> {
        Ok(Arc::new(self.slice_of(path)?))
    }

    fn open_read(&self, path: &Path) -> Result<FileSlice, OpenReadError> {
        Ok(FileSlice::new(Arc::new(self.slice_of(path)?)))
    }

    fn atomic_read(&self, path: &Path) -> Result<Vec<u8>, OpenReadError> {
        Ok(self.slice_of(path)?.as_slice().to_vec())
    }

    fn exists(&self, path: &Path) -> Result<bool, OpenReadError> {
        Ok(self.files.contains_key(path))
    }

    fn delete(&self, path: &Path) -> Result<(), DeleteError> {
        Err(DeleteError::IoError {
            io_error: Arc::new(Self::read_only(path)),
            filepath: path.to_path_buf(),
        })
    }

    fn open_write(&self, path: &Path) -> Result<WritePtr, OpenWriteError> {
        Err(OpenWriteError::IoError {
            io_error: Arc::new(Self::read_only(path)),
            filepath: path.to_path_buf(),
        })
    }

    fn atomic_write(&self, path: &Path, _data: &[u8]) -> io::Result<()> {
        Err(Self::read_only(path))
    }

    fn sync_directory(&self) -> io::Result<()> {
        Ok(())
    }

    /// Granted without a lockfile. A lock guards an index against a second
    /// writer; a snapshot's bytes are immutable and belong to this handle
    /// alone, so there is nothing to guard — and the default implementation
    /// would try to create the lockfile through `open_write`, which a
    /// read-only directory refuses.
    fn acquire_lock(&self, _lock: &ld_lucivy::directory::Lock)
        -> Result<ld_lucivy::directory::DirectoryLock, ld_lucivy::directory::error::LockError>
    {
        Ok(ld_lucivy::directory::DirectoryLock::from(Box::new(())))
    }

    fn watch(&self, watch_callback: WatchCallback) -> ld_lucivy::Result<WatchHandle> {
        Ok(self.watch_router.write().unwrap().subscribe(watch_callback))
    }

}

/// Shard storage backed by a LUCE snapshot held in memory: read-only, and
/// serving slices of the one blob.
///
/// `ShardedHandle::open_snapshot` builds one of these; nothing writes through
/// it, so the write side answers with the reason rather than a generic error.
pub struct SnapshotShardStorage {
    /// Shard index → its directory. Ordered by the snapshot's `shard_N` names,
    /// so shard 0 is the snapshot's shard_0.
    shards: Vec<SnapshotDirectory>,
    root: HashMap<String, OwnedBytes>,
}

impl SnapshotShardStorage {
    /// Read a snapshot's table of contents and prepare its shard directories.
    /// The blob is not copied: every directory slices the same bytes.
    pub fn open(blob: OwnedBytes) -> Result<Self, String> {
        let blob_len = blob.len();
        let (dirs, root_files) = SnapshotDirectory::open_all(blob)?;
        if dirs.is_empty() {
            return Err("snapshot holds no index".into());
        }
        // `shard_N` in numeric order; anything else keeps the snapshot's order.
        let mut ordered: Vec<(usize, SnapshotDirectory)> = dirs
            .into_iter()
            .enumerate()
            .map(|(i, (path, dir))| {
                let n = path.strip_prefix("shard_").and_then(|n| n.parse::<usize>().ok());
                (n.unwrap_or(i), dir)
            })
            .collect();
        ordered.sort_by_key(|(n, _)| *n);
        let shards: Vec<SnapshotDirectory> = ordered.into_iter().map(|(_, d)| d).collect();
        let files: usize = shards.iter().map(|d| d.num_files()).sum();
        if std::env::var("LUCIVY_VERBOSE").is_ok() {
            eprintln!(
                "[snapshot] {} shards, {files} files, {} MB — served from the blob, not extracted",
                shards.len(), blob_len >> 20
            );
        }
        Ok(Self {
            shards,
            root: root_files.into_iter().collect(),
        })
    }

    pub fn num_shards(&self) -> usize {
        self.shards.len()
    }

    /// Bytes the snapshot occupies — the whole cost of serving this index.
    pub fn blob_len(&self) -> usize {
        self.shards.first().map(|d| d.blob_len()).unwrap_or(0)
    }
}

impl crate::sharded_handle::ShardStorage for SnapshotShardStorage {
    fn create_shard_handle(
        &self,
        _shard_id: usize,
        _config: &crate::query::SchemaConfig,
    ) -> Result<crate::handle::LucivyHandle, String> {
        Err("a snapshot is read-only: create the index in a writable directory, \
             package it, and serve the package".into())
    }

    fn open_shard_handle(&self, shard_id: usize) -> Result<crate::handle::LucivyHandle, String> {
        let dir = self
            .shards
            .get(shard_id)
            .ok_or_else(|| format!("snapshot has no shard {shard_id}"))?
            .clone();
        crate::handle::LucivyHandle::open(dir)
    }

    fn write_root_file(&self, name: &str, _data: &[u8]) -> Result<(), String> {
        Err(format!("a snapshot is read-only: cannot write {name}"))
    }

    fn read_root_file(&self, name: &str) -> Result<Vec<u8>, String> {
        self.root
            .get(name)
            .map(|b| b.as_slice().to_vec())
            .ok_or_else(|| format!("snapshot has no root file {name}"))
    }

    fn root_file_exists(&self, name: &str) -> bool {
        self.root.contains_key(name)
    }
}
