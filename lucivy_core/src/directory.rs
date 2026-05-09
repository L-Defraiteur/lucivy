//! Platform-agnostic Directory implementations.
//!
//! - `StdFsDirectory` — buffered fs::read/write. Used on WASM (Emscripten VFS).
//! - `NativeDirectory` — `MmapDirectory` on native (zero-copy reads via mmap),
//!   falls back to `StdFsDirectory` on WASM where mmap is unavailable.

use std::fs;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use ld_lucivy::directory::error::{DeleteError, OpenReadError, OpenWriteError};
use ld_lucivy::directory::{
    AntiCallToken, Directory, FileHandle, FileSlice, TerminatingWrite, WatchCallback,
    WatchCallbackList, WatchHandle, WritePtr,
};

/// A simple Directory implementation backed by std::fs.
///
/// On native platforms, files are stored on the real filesystem.
/// On Emscripten, std::fs calls go through the Emscripten VFS (MEMFS),
/// which can be persisted to IndexedDB via FS.syncfs().
#[derive(Clone)]
pub struct StdFsDirectory {
    root: PathBuf,
    watch_router: Arc<RwLock<WatchCallbackList>>,
}

impl std::fmt::Debug for StdFsDirectory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "StdFsDirectory({:?})", self.root)
    }
}

impl StdFsDirectory {
    pub fn open(path: impl Into<PathBuf>) -> io::Result<Self> {
        let root = path.into();
        fs::create_dir_all(&root)?;
        Ok(Self {
            root,
            watch_router: Arc::new(RwLock::new(WatchCallbackList::default())),
        })
    }

    fn resolve(&self, path: &Path) -> PathBuf {
        self.root.join(path)
    }
}

/// Writer that buffers ALL writes in memory. Filesystem I/O happens only at
/// terminate() — never during write() or flush(). This is critical for WASM/OPFS
/// where synchronous I/O is slow and would block scheduler threads.
///
/// Memory is bounded by the indexer's mem_budget which triggers finalize
/// (and thus terminate) before segments grow too large.
struct FsWriter {
    path: PathBuf,
    buffer: Vec<u8>,
    written_to_disk: bool,
}

impl FsWriter {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            buffer: Vec::new(),
            written_to_disk: false,
        }
    }
}

impl Write for FsWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buffer.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        // No-op: all data stays in RAM until terminate().
        // The Directory contract says "writes may be aggressively buffered".
        // Durability is guaranteed by terminate() called during finalize/commit.
        Ok(())
    }
}

impl TerminatingWrite for FsWriter {
    fn terminate_ref(&mut self, _: AntiCallToken) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            if !parent.exists() {
                fs::create_dir_all(parent)?;
            }
        }
        fs::write(&self.path, &self.buffer)?;
        self.written_to_disk = true;
        Ok(())
    }
}

impl Drop for FsWriter {
    fn drop(&mut self) {
        if !self.written_to_disk && !self.buffer.is_empty() {
            eprintln!(
                "Warning: FsWriter for {:?} dropped with {} bytes unwritten.",
                self.path, self.buffer.len()
            );
        }
    }
}

impl Directory for StdFsDirectory {
    fn get_file_handle(&self, path: &Path) -> Result<Arc<dyn FileHandle>, OpenReadError> {
        // FileSlice implements FileHandle, same approach as RamDirectory.
        let file_slice = self.open_read(path)?;
        Ok(Arc::new(file_slice))
    }

    fn open_read(&self, path: &Path) -> Result<FileSlice, OpenReadError> {
        let full = self.resolve(path);
        let data = fs::read(&full).map_err(|e| {
            if e.kind() == io::ErrorKind::NotFound {
                OpenReadError::FileDoesNotExist(full.clone())
            } else {
                OpenReadError::IoError {
                    io_error: Arc::new(e),
                    filepath: full.clone(),
                }
            }
        })?;
        Ok(FileSlice::from(data))
    }

    fn open_write(&self, path: &Path) -> Result<WritePtr, OpenWriteError> {
        let full = self.resolve(path);
        // No I/O here — existence check and dir creation are deferred to
        // FsWriter::terminate(). The WORM contract (enforced by ManagedDirectory)
        // guarantees callers don't create the same file twice.
        Ok(BufWriter::new(Box::new(FsWriter::new(full))))
    }

    fn delete(&self, path: &Path) -> Result<(), DeleteError> {
        let full = self.resolve(path);
        fs::remove_file(&full).map_err(|e| {
            if e.kind() == io::ErrorKind::NotFound {
                DeleteError::FileDoesNotExist(full)
            } else {
                DeleteError::IoError {
                    io_error: Arc::new(e),
                    filepath: full,
                }
            }
        })
    }

    fn exists(&self, path: &Path) -> Result<bool, OpenReadError> {
        Ok(self.resolve(path).exists())
    }

    fn atomic_read(&self, path: &Path) -> Result<Vec<u8>, OpenReadError> {
        let full = self.resolve(path);
        fs::read(&full).map_err(|e| {
            if e.kind() == io::ErrorKind::NotFound {
                OpenReadError::FileDoesNotExist(full.clone())
            } else {
                OpenReadError::IoError {
                    io_error: Arc::new(e),
                    filepath: full.clone(),
                }
            }
        })
    }

    fn atomic_write(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        let full = self.resolve(path);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&full, data)?;
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
        // On native: we could fsync the directory fd for durability.
        // On Emscripten: persistence is handled by FS.syncfs() on the JS side.
        Ok(())
    }
}

// ── NativeDirectory: best directory for each platform ─────────────────────

/// On native: MmapDirectory (zero-copy reads via mmap, file watcher).
/// On WASM: StdFsDirectory (buffered I/O via Emscripten VFS).
#[cfg(all(feature = "mmap", not(target_arch = "wasm32")))]
pub type NativeDirectory = ld_lucivy::directory::MmapDirectory;

#[cfg(any(not(feature = "mmap"), target_arch = "wasm32"))]
pub type NativeDirectory = StdFsDirectory;
