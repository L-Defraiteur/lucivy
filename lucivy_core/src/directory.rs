//! Platform-agnostic Directory implementation using std::fs.
//!
//! Works on all targets:
//! - Native (Linux, macOS, Windows): real filesystem
//! - Emscripten (WASM): Emscripten VFS (MEMFS/IDBFS)
//!
//! For native targets, MmapDirectory would be faster for large indexes.
//! This can be swapped in later as an optimization.

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

/// Writer that buffers writes in memory and flushes to the filesystem.
struct FsWriter {
    path: PathBuf,
    buffer: Vec<u8>,
    is_flushed: bool,
}

impl FsWriter {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            buffer: Vec::new(),
            is_flushed: true,
        }
    }
}

impl Write for FsWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.is_flushed = false;
        self.buffer.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.is_flushed = true;
        fs::write(&self.path, &self.buffer)
    }
}

impl TerminatingWrite for FsWriter {
    fn terminate_ref(&mut self, _: AntiCallToken) -> io::Result<()> {
        self.flush()
    }
}

impl Drop for FsWriter {
    fn drop(&mut self) {
        if !self.is_flushed {
            eprintln!(
                "Warning: FsWriter for {:?} dropped without flushing. Data may be lost.",
                self.path
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
        if full.exists() {
            return Err(OpenWriteError::FileAlreadyExists(full));
        }
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).map_err(|e| OpenWriteError::IoError {
                io_error: Arc::new(e),
                filepath: full.clone(),
            })?;
        }
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
