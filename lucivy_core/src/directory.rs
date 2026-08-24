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

// ── Lazy reads with a bounded whole-file cache ───────────────────────────
//
// Opening a segment reads nothing: the handle only stats the file. The
// first real read materialises the file into a process-wide LRU cache
// (budget `LUCIVY_FILE_CACHE_BYTES`, 768 MB on wasm32, 4 GB elsewhere) and
// every read after that is an Arc slice. Small reads (footers and headers
// probed at open, up to 64 KB) are served straight from the file without
// pulling it in. This is what mmap gives natively — pay for what you touch,
// let the cold files go — done at file granularity for WASM, where every
// `fs::read` at open kept all sidecars of every segment resident (837 MB
// of sidecars for 2,000 kernel files, copied once more by every reader).

const SMALL_READ_MAX: usize = 64 * 1024;

fn file_cache_budget() -> usize {
    static BUDGET: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *BUDGET.get_or_init(|| {
        std::env::var("LUCIVY_FILE_CACHE_BYTES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(if cfg!(target_arch = "wasm32") { 768 << 20 } else { 4 << 30 })
    })
}

struct FileCache {
    entries: std::collections::HashMap<PathBuf, (ld_lucivy::directory::OwnedBytes, u64)>,
    total: usize,
    tick: u64,
}

impl FileCache {
    fn global() -> &'static std::sync::Mutex<FileCache> {
        static CACHE: std::sync::OnceLock<std::sync::Mutex<FileCache>> = std::sync::OnceLock::new();
        CACHE.get_or_init(|| {
            std::sync::Mutex::new(FileCache {
                entries: std::collections::HashMap::new(),
                total: 0,
                tick: 0,
            })
        })
    }

    fn get(&mut self, path: &Path) -> Option<ld_lucivy::directory::OwnedBytes> {
        self.tick += 1;
        let tick = self.tick;
        self.entries.get_mut(path).map(|(bytes, last)| {
            *last = tick;
            bytes.clone()
        })
    }

    fn insert(&mut self, path: PathBuf, bytes: ld_lucivy::directory::OwnedBytes) {
        let budget = file_cache_budget();
        let len = bytes.len();
        while self.total + len > budget && !self.entries.is_empty() {
            let victim = self
                .entries
                .iter()
                .min_by_key(|(_, (_, last))| *last)
                .map(|(p, _)| p.clone())
                .unwrap();
            if let Some((evicted, _)) = self.entries.remove(&victim) {
                self.total -= evicted.len();
            }
        }
        self.tick += 1;
        self.total += len;
        if let Some((old, _)) = self.entries.insert(path, (bytes, self.tick)) {
            self.total -= old.len();
        }
    }

    fn remove(&mut self, path: &Path) {
        if let Some((old, _)) = self.entries.remove(path) {
            self.total -= old.len();
        }
    }
}

/// Read handle over a file on disk that is read only when asked to.
struct LazyFsHandle {
    path: PathBuf,
    len: usize,
}

impl std::fmt::Debug for LazyFsHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "LazyFsHandle({}, {} bytes)", self.path.display(), self.len)
    }
}

impl ld_lucivy::HasLen for LazyFsHandle {
    fn len(&self) -> usize {
        self.len
    }
}

impl LazyFsHandle {
    fn whole(&self) -> io::Result<ld_lucivy::directory::OwnedBytes> {
        if let Some(bytes) = FileCache::global().lock().unwrap().get(&self.path) {
            return Ok(bytes);
        }
        let data = fs::read(&self.path)?;
        let bytes = ld_lucivy::directory::OwnedBytes::new(data);
        FileCache::global().lock().unwrap().insert(self.path.clone(), bytes.clone());
        Ok(bytes)
    }
}

impl FileHandle for LazyFsHandle {
    fn read_bytes(&self, range: std::ops::Range<usize>) -> io::Result<ld_lucivy::directory::OwnedBytes> {
        let range = range.start.min(self.len)..range.end.min(self.len);
        if range.end - range.start <= SMALL_READ_MAX && range.end - range.start < self.len {
            if let Some(bytes) = FileCache::global().lock().unwrap().get(&self.path) {
                return Ok(bytes.slice(range));
            }
            use std::io::{Read, Seek, SeekFrom};
            let mut file = fs::File::open(&self.path)?;
            file.seek(SeekFrom::Start(range.start as u64))?;
            let mut buf = vec![0u8; range.end - range.start];
            file.read_exact(&mut buf)?;
            return Ok(ld_lucivy::directory::OwnedBytes::new(buf));
        }
        Ok(self.whole()?.slice(range))
    }
}

impl Directory for StdFsDirectory {
    fn get_file_handle(&self, path: &Path) -> Result<Arc<dyn FileHandle>, OpenReadError> {
        let full = self.resolve(path);
        let len = fs::metadata(&full)
            .map_err(|e| {
                if e.kind() == io::ErrorKind::NotFound {
                    OpenReadError::FileDoesNotExist(full.clone())
                } else {
                    OpenReadError::IoError {
                        io_error: Arc::new(e),
                        filepath: full.clone(),
                    }
                }
            })?
            .len() as usize;
        Ok(Arc::new(LazyFsHandle { path: full, len }))
    }

    fn open_read(&self, path: &Path) -> Result<FileSlice, OpenReadError> {
        Ok(FileSlice::new(self.get_file_handle(path)?))
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
        FileCache::global().lock().unwrap().remove(&full);
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
        // Temp file + rename: `fs::write` truncates before it writes, and a
        // reader reloading between the two sees an empty meta.json
        // ("Meta file cannot be deserialized … Content: \"\""). Rare while
        // metas were saved once per commit; routine now that every finished
        // merge saves them. Rename is atomic on POSIX and on Emscripten's FS.
        let tmp = full.with_extension(format!(
            "{}.tmp.{}",
            full.extension().and_then(|e| e.to_str()).unwrap_or(""),
            std::process::id()));
        // Step trace under LUCIVY_VERBOSE: on WASM every step is a proxied
        // OPFS operation, and a hang has to be pinned to one of them.
        let verbose = std::env::var("LUCIVY_VERBOSE").is_ok();
        let t0 = std::time::Instant::now();
        if verbose { eprintln!("[fs] atomic_write {} ({} B): write tmp", full.display(), data.len()); }
        fs::write(&tmp, data)?;
        if verbose { eprintln!("[fs] atomic_write {}: rename ({:.1}ms)", full.display(), t0.elapsed().as_secs_f64() * 1e3); }
        if let Err(e) = fs::rename(&tmp, &full) {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
        FileCache::global().lock().unwrap().remove(&full);
        if verbose { eprintln!("[fs] atomic_write {}: done ({:.1}ms)", full.display(), t0.elapsed().as_secs_f64() * 1e3); }
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
