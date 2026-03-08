//! In-memory Directory with import/export for OPFS sync.
//!
//! Files live in RAM. The OPFS sync happens at the JS boundary:
//! - On open: JS reads OPFS files → passes to `import_file()`
//! - On commit: Rust returns dirty files via `export_dirty()` → JS writes to OPFS
//!
//! This keeps the Directory trait synchronous (tantivy requirement)
//! while enabling async OPFS persistence in the browser.

use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use ld_lucivy::directory::error::{DeleteError, OpenReadError, OpenWriteError};
use ld_lucivy::directory::{
    AntiCallToken, Directory, FileHandle, FileSlice, TerminatingWrite, WatchCallback,
    WatchCallbackList, WatchHandle, WritePtr,
};

/// In-memory directory that tracks dirty files for OPFS sync.
#[derive(Clone)]
pub struct MemoryDirectory {
    inner: Arc<RwLock<MemoryDirectoryInner>>,
    watch_router: Arc<RwLock<WatchCallbackList>>,
}

struct MemoryDirectoryInner {
    files: HashMap<PathBuf, Vec<u8>>,
    dirty: HashSet<PathBuf>,
    deleted: HashSet<PathBuf>,
}

impl std::fmt::Debug for MemoryDirectory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "MemoryDirectory")
    }
}

impl MemoryDirectory {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(MemoryDirectoryInner {
                files: HashMap::new(),
                dirty: HashSet::new(),
                deleted: HashSet::new(),
            })),
            watch_router: Arc::new(RwLock::new(WatchCallbackList::default())),
        }
    }

    /// Import a file (called during index open, before tantivy touches anything).
    pub fn import_file(&self, path: &str, data: Vec<u8>) {
        let mut inner = self.inner.write().unwrap();
        inner.files.insert(PathBuf::from(path), data);
    }

    /// Export dirty files since last call (for OPFS sync after commit).
    /// Returns (modified files, deleted paths) and clears the dirty/deleted sets.
    pub fn export_dirty(&self) -> (Vec<(String, Vec<u8>)>, Vec<String>) {
        let mut inner = self.inner.write().unwrap();

        let modified: Vec<(String, Vec<u8>)> = inner
            .dirty
            .iter()
            .filter_map(|path| {
                let data = inner.files.get(path)?;
                Some((path.to_string_lossy().to_string(), data.clone()))
            })
            .collect();

        let deleted: Vec<String> = inner
            .deleted
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();

        inner.dirty.clear();
        inner.deleted.clear();

        (modified, deleted)
    }

    /// Export ALL files (for full OPFS sync, e.g. after create).
    pub fn export_all(&self) -> Vec<(String, Vec<u8>)> {
        let inner = self.inner.read().unwrap();
        inner
            .files
            .iter()
            .map(|(path, data)| (path.to_string_lossy().to_string(), data.clone()))
            .collect()
    }
}

impl Directory for MemoryDirectory {
    fn get_file_handle(&self, path: &Path) -> Result<Arc<dyn FileHandle>, OpenReadError> {
        let file_slice = self.open_read(path)?;
        Ok(Arc::new(file_slice))
    }

    fn open_read(&self, path: &Path) -> Result<FileSlice, OpenReadError> {
        let inner = self.inner.read().unwrap();
        let data = inner
            .files
            .get(path)
            .ok_or_else(|| OpenReadError::FileDoesNotExist(path.to_path_buf()))?;
        Ok(FileSlice::from(data.clone()))
    }

    fn open_write(&self, path: &Path) -> Result<WritePtr, OpenWriteError> {
        let inner = self.inner.read().unwrap();
        if inner.files.contains_key(path) {
            return Err(OpenWriteError::FileAlreadyExists(path.to_path_buf()));
        }
        drop(inner);

        let writer = MemoryWriter {
            dir: self.inner.clone(),
            path: path.to_path_buf(),
            buffer: Vec::new(),
            is_flushed: true,
        };
        Ok(io::BufWriter::new(Box::new(writer)))
    }

    fn delete(&self, path: &Path) -> Result<(), DeleteError> {
        let mut inner = self.inner.write().unwrap();
        if inner.files.remove(path).is_none() {
            return Err(DeleteError::FileDoesNotExist(path.to_path_buf()));
        }
        inner.dirty.remove(path);
        inner.deleted.insert(path.to_path_buf());
        Ok(())
    }

    fn exists(&self, path: &Path) -> Result<bool, OpenReadError> {
        let inner = self.inner.read().unwrap();
        Ok(inner.files.contains_key(path))
    }

    fn atomic_read(&self, path: &Path) -> Result<Vec<u8>, OpenReadError> {
        let inner = self.inner.read().unwrap();
        inner
            .files
            .get(path)
            .cloned()
            .ok_or_else(|| OpenReadError::FileDoesNotExist(path.to_path_buf()))
    }

    fn atomic_write(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        let mut inner = self.inner.write().unwrap();
        inner.files.insert(path.to_path_buf(), data.to_vec());
        inner.dirty.insert(path.to_path_buf());
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
        Ok(())
    }
}

/// Writer that buffers in memory, flushes to the MemoryDirectory on terminate.
struct MemoryWriter {
    dir: Arc<RwLock<MemoryDirectoryInner>>,
    path: PathBuf,
    buffer: Vec<u8>,
    is_flushed: bool,
}

impl Write for MemoryWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.is_flushed = false;
        self.buffer.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.is_flushed = true;
        let mut inner = self.dir.write().unwrap();
        inner.files.insert(self.path.clone(), self.buffer.clone());
        inner.dirty.insert(self.path.clone());
        Ok(())
    }
}

impl TerminatingWrite for MemoryWriter {
    fn terminate_ref(&mut self, _: AntiCallToken) -> io::Result<()> {
        self.flush()
    }
}
