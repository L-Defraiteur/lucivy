//! **Tagged generations** (lucivy 4.4, the B way of `docs/lucivy/20-11-2026/02-conception-lucivy-4-4-la-voie-b.md` in
//! rag3db). A caller whose database names a generation of the index tags each commit with it (`commit_tagged`),
//! keeps on disk the last generation its database validated and the ones above (`release_below` lets the others go),
//! brings the four shards back to one of them (`revert_to`, all or nothing), or opens one for reading without
//! touching the directory (`open_read_only_at`).
//!
//! On disk, per shard: `meta.<g>.json`, a copy of `meta.json` taken at each tagged commit, and `generations.json`,
//! `{"kept": [...], "current": g}`. At the root: `_shard_stats.<g>.bin` and `_generations.json`, the same list. A
//! shard's own generation is the payload of its `meta.json` (`lucivy-generation:<g>`), which merges and compactions
//! carry over: a revert cut between two shards is seen. The garbage collection and the deletion of consumed
//! dictionary pairs spare whatever a kept meta names (`ld_lucivy`), and `meta.json` carries `min_lucivy` so that an
//! older lucivy refuses the index instead of collecting the kept generations.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ld_lucivy::directory::error::{DeleteError, LockError, OpenReadError, OpenWriteError};
use ld_lucivy::directory::{DirectoryLock, FileHandle, Lock, WatchCallback, WatchHandle, WritePtr};
use ld_lucivy::Directory;

use crate::handle::LucivyHandle;
use crate::shard_router::ShardRouter;
use crate::sharded_handle::{ShardStorage, ShardedHandle, SHARD_STATS_FILE};

const ROOT_GENERATIONS: &str = "_generations.json";
const PAYLOAD_PREFIX: &str = "lucivy-generation:";

fn stats_of(g: u64) -> String {
    format!("_shard_stats.{g}.bin")
}

fn state_json(kept: &[u64], current: u64) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({ "kept": kept, "current": current })).unwrap()
}

fn parse_state(bytes: &[u8]) -> Result<(Vec<u64>, u64), String> {
    let v: serde_json::Value = serde_json::from_slice(bytes).map_err(|e| format!("{ROOT_GENERATIONS}: {e}"))?;
    let kept = v["kept"].as_array().map(|a| a.iter().filter_map(|g| g.as_u64()).collect()).unwrap_or_default();
    Ok((kept, v["current"].as_u64().unwrap_or(0)))
}

/// The generation a shard's `meta.json` carries, if tagged.
fn shard_generation(shard: &LucivyHandle) -> Result<Option<u64>, String> {
    let metas = shard.index.load_metas().map_err(|e| format!("meta.json: {e}"))?;
    Ok(metas.payload.as_deref().and_then(|p| p.strip_prefix(PAYLOAD_PREFIX)).and_then(|g| g.parse().ok()))
}

impl ShardedHandle {
    /// Whether this index commits by tagged generations.
    pub fn is_tagged(&self) -> bool {
        self.storage.root_file_exists(ROOT_GENERATIONS)
    }

    fn tagged_state(&self) -> Result<(Vec<u64>, u64), String> {
        if !self.is_tagged() {
            return Ok((Vec::new(), 0));
        }
        parse_state(&self.storage.read_root_file(ROOT_GENERATIONS)?)
    }

    fn write_shard_state(&self, kept: &[u64], current: u64) -> Result<(), String> {
        for (i, shard) in self.shards.iter().enumerate() {
            shard.index.directory().atomic_write(Path::new(ld_lucivy::index::TAGGED_GENERATIONS_FILE), &state_json(kept, current))
                .map_err(|e| format!("shard_{i}: generations.json: {e}"))?;
        }
        Ok(())
    }

    /// Commits every shard as generation `g`, strictly above the current one. The tagged generations from the last
    /// released one up stay on disk until `release_below`.
    pub fn commit_tagged(&self, g: u64) -> Result<(), String> {
        let (mut kept, current) = self.tagged_state()?;
        if g <= current && !kept.is_empty() {
            return Err(format!("commit_tagged({g}): the index is at generation {current}, a tagged commit goes above it"));
        }
        // The shards are marked tagged before their commit, so that its meta.json already says min_lucivy.
        self.write_shard_state(&kept, current)?;
        for shard in &self.shards {
            *shard.next_payload.lock().map_err(|_| "payload lock poisoned")? = Some(format!("{PAYLOAD_PREFIX}{g}"));
        }
        let committed = self.commit_inner();
        for shard in &self.shards {
            shard.next_payload.lock().map_err(|_| "payload lock poisoned")?.take();
        }
        committed?;
        // generations.json names g before meta.<g>.json is written: a merge that collects in between keeps it.
        kept.push(g);
        self.write_shard_state(&kept, g)?;
        for (i, shard) in self.shards.iter().enumerate() {
            let dir = shard.index.directory();
            let meta = dir.atomic_read(Path::new("meta.json")).map_err(|e| format!("shard_{i}: meta.json: {e}"))?;
            dir.atomic_write(&ld_lucivy::index::tagged_meta_path(g), &meta).map_err(|e| format!("shard_{i}: meta.{g}.json: {e}"))?;
        }
        let stats = self.storage.read_root_file(SHARD_STATS_FILE)?;
        self.storage.write_root_file(&stats_of(g), &stats)?;
        self.storage.write_root_file(ROOT_GENERATIONS, &state_json(&kept, g))
    }

    /// The caller's database has durably validated `g`: every tagged generation below `g` goes; their files leave at
    /// the next garbage collection. Never `g` itself, nor anything above.
    pub fn release_below(&self, g: u64) -> Result<(), String> {
        let (kept, current) = self.tagged_state()?;
        let (gone, stay): (Vec<u64>, Vec<u64>) = kept.into_iter().partition(|k| *k < g);
        self.write_shard_state(&stay, current)?;
        self.storage.write_root_file(ROOT_GENERATIONS, &state_json(&stay, current))?;
        for k in gone {
            for shard in &self.shards {
                match shard.index.directory().delete(&ld_lucivy::index::tagged_meta_path(k)) {
                    Ok(()) | Err(DeleteError::FileDoesNotExist(_)) => {}
                    Err(e) => return Err(format!("meta.{k}.json: {e}")),
                }
            }
            self.storage.delete_root_file(&stats_of(k))?;
        }
        Ok(())
    }

    /// Brings one shard back to the meta it is given, and reopens it from the disk.
    fn set_shard_meta(shard: &LucivyHandle, meta: &[u8]) -> Result<(), String> {
        shard.reopen_writer_after(|| {
            shard.index.directory().atomic_write(Path::new("meta.json"), meta).map_err(|e| format!("meta.json: {e}"))?;
            shard.index.reload_sfx_dictionary().map_err(|e| format!("dictionary: {e}"))
        })?;
        shard.reader.reload().map_err(|e| format!("reader: {e}"))
    }

    /// Brings the four shards back to generation `g`, kept on disk: all or nothing. The generations above are
    /// forgotten, their meta files deleted, their documents gone with their segments.
    pub fn revert_to(&self, g: u64) -> Result<(), String> {
        self.ensure_writable_for_tagged()?;
        let (kept, _) = self.tagged_state()?;
        if !kept.contains(&g) {
            return Err(format!("revert_to({g}): not a kept generation (kept: {kept:?})"));
        }
        let mut done: Vec<(usize, Vec<u8>)> = Vec::new();
        for (i, shard) in self.shards.iter().enumerate() {
            let dir = shard.index.directory();
            let step = (|| -> Result<Vec<u8>, String> {
                #[cfg(feature = "failpoints")]
                if fail::eval("tagged::revert_shard", |_| ()).is_some() {
                    return Err(format!("shard_{i}: injected failure"));
                }
                let before = dir.atomic_read(Path::new("meta.json")).map_err(|e| format!("shard_{i}: meta.json: {e}"))?;
                let target = dir.atomic_read(&ld_lucivy::index::tagged_meta_path(g)).map_err(|e| format!("shard_{i}: meta.{g}.json: {e}"))?;
                Self::set_shard_meta(shard, &target).map_err(|e| format!("shard_{i}: {e}"))?;
                Ok(before)
            })();
            match step {
                Ok(before) => done.push((i, before)),
                Err(e) => {
                    // All or nothing: the shards already back go forward again.
                    for (j, before) in done.into_iter().rev() {
                        Self::set_shard_meta(&self.shards[j], &before).map_err(|e2| format!("{e}; and shard_{j} could not go forward again: {e2}"))?;
                    }
                    return Err(format!("revert_to({g}): {e}; every shard is left where it was"));
                }
            }
        }
        let stats = self.storage.read_root_file(&stats_of(g))?;
        self.storage.write_root_file(SHARD_STATS_FILE, &stats)?;
        *self.router.lock().map_err(|_| "router lock poisoned")? = ShardRouter::from_bytes(&stats)?;
        let (below, above): (Vec<u64>, Vec<u64>) = kept.into_iter().partition(|k| *k <= g);
        self.write_shard_state(&below, g)?;
        self.storage.write_root_file(ROOT_GENERATIONS, &state_json(&below, g))?;
        for k in above {
            for shard in &self.shards {
                let _ = shard.index.directory().delete(&ld_lucivy::index::tagged_meta_path(k));
            }
            self.storage.delete_root_file(&stats_of(k))?;
        }
        Ok(())
    }

    /// A commit that does not change what the index holds for its caller (a maintenance: purge, compaction): on a
    /// tagged index, its meta.json keeps the current generation in its payload, and no new generation is made.
    /// Untagged, a plain commit.
    pub(crate) fn commit_keeping_generation(&self) -> Result<(), String> {
        if !self.is_tagged() {
            return self.commit_inner();
        }
        let (_, current) = self.tagged_state()?;
        for shard in &self.shards {
            *shard.next_payload.lock().map_err(|_| "payload lock poisoned")? = Some(format!("{PAYLOAD_PREFIX}{current}"));
        }
        let committed = self.commit_inner();
        for shard in &self.shards {
            shard.next_payload.lock().map_err(|_| "payload lock poisoned")?.take();
        }
        committed
    }

    /// A maintenance of a tagged index commits nothing of its caller's: documents not yet committed would land
    /// outside every generation.
    pub(crate) fn ensure_no_untagged_documents(&self) -> Result<(), String> {
        if self.is_tagged() && self.shards.iter().any(|s| s.has_uncommitted()) {
            return Err("this index is tagged (lucivy 4.4): commit_tagged(<generation>) the pending documents before a maintenance".into());
        }
        Ok(())
    }

    fn ensure_writable_for_tagged(&self) -> Result<(), String> {
        if self.read_only {
            return Err("a read-only handle does not revert".into());
        }
        if self.shards.iter().any(|s| s.has_uncommitted()) {
            return Err("uncommitted documents: commit or drop them before a revert".into());
        }
        Ok(())
    }

    /// The tagged generations on disk, ascending, and the current one. A shard whose `meta.json` names another
    /// generation than the directory (a revert cut between two shards) is said, by name.
    pub fn tagged_generations(&self) -> Result<(Vec<u64>, u64), String> {
        let (kept, current) = self.tagged_state()?;
        if kept.is_empty() {
            return Ok((kept, current));
        }
        for (i, shard) in self.shards.iter().enumerate() {
            let at = shard_generation(shard)?;
            if at != Some(current) {
                return Err(format!(
                    "shard_{i} is at generation {at:?} and the directory at {current}: a revert was cut — revert_to({current}) or rebuild"
                ));
            }
        }
        Ok((kept, current))
    }

    /// Opens generation `g` of a directory for reading only: nothing written, nothing deleted.
    pub fn open_read_only_at(path: &str, g: u64) -> Result<ShardedHandle, String> {
        let storage = GenerationStorage { base: PathBuf::from(path), g };
        if !storage.root_file_exists(&stats_of(g)) {
            return Err(format!("generation {g} is not kept in {path}"));
        }
        let mut handle = ShardedHandle::open_with_storage(Box::new(storage))?;
        handle.read_only = true;
        Ok(handle)
    }
}

/// The storage of one kept generation, read-only.
struct GenerationStorage {
    base: PathBuf,
    g: u64,
}

impl GenerationStorage {
    fn root_name(&self, name: &str) -> String {
        if name == SHARD_STATS_FILE { stats_of(self.g) } else { name.to_string() }
    }
}

impl ShardStorage for GenerationStorage {
    fn create_shard_handle(&self, _shard_id: usize, _config: &crate::query::SchemaConfig) -> Result<LucivyHandle, String> {
        Err("a kept generation is read-only".into())
    }

    fn open_shard_handle(&self, shard_id: usize) -> Result<LucivyHandle, String> {
        let shard_dir = self.base.join(format!("shard_{shard_id}"));
        let inner = crate::directory::NativeDirectory::open(shard_dir.to_str().unwrap())
            .map_err(|e| format!("cannot open shard_{shard_id} dir: {e}"))?;
        if !shard_dir.join(ld_lucivy::index::tagged_meta_path(self.g)).exists() {
            return Err(format!("shard_{shard_id} does not keep generation {}", self.g));
        }
        LucivyHandle::open_read_only(GenerationDirectory { inner: Box::new(inner), meta: ld_lucivy::index::tagged_meta_path(self.g) })
    }

    fn write_root_file(&self, name: &str, _data: &[u8]) -> Result<(), String> {
        Err(format!("a kept generation is read-only: cannot write {name}"))
    }

    fn read_root_file(&self, name: &str) -> Result<Vec<u8>, String> {
        std::fs::read(self.base.join(self.root_name(name))).map_err(|e| format!("cannot read {name}: {e}"))
    }

    fn root_file_exists(&self, name: &str) -> bool {
        self.base.join(self.root_name(name)).exists()
    }

    fn drop_storage(&self, _num_shards: usize) -> Result<(), String> {
        Err("a kept generation is read-only".into())
    }
}

/// A shard directory seen at one kept generation: `meta.json` is read from `meta.<g>.json`, nothing is written.
#[derive(Clone, Debug)]
struct GenerationDirectory {
    inner: Box<dyn Directory>,
    meta: PathBuf,
}

impl GenerationDirectory {
    fn map<'a>(&'a self, path: &'a Path) -> &'a Path {
        if path == Path::new("meta.json") { &self.meta } else { path }
    }

    fn read_only(path: &Path) -> std::io::Error {
        std::io::Error::other(format!("a kept generation is read-only: {}", path.display()))
    }
}

impl Directory for GenerationDirectory {
    fn get_file_handle(&self, path: &Path) -> Result<Arc<dyn FileHandle>, OpenReadError> {
        self.inner.get_file_handle(self.map(path))
    }

    fn delete(&self, path: &Path) -> Result<(), DeleteError> {
        Err(DeleteError::IoError { io_error: Arc::new(Self::read_only(path)), filepath: path.to_path_buf() })
    }

    fn exists(&self, path: &Path) -> Result<bool, OpenReadError> {
        self.inner.exists(self.map(path))
    }

    fn open_write(&self, path: &Path) -> Result<WritePtr, OpenWriteError> {
        Err(OpenWriteError::IoError { io_error: Arc::new(Self::read_only(path)), filepath: path.to_path_buf() })
    }

    fn atomic_read(&self, path: &Path) -> Result<Vec<u8>, OpenReadError> {
        self.inner.atomic_read(self.map(path))
    }

    fn atomic_write(&self, path: &Path, _data: &[u8]) -> std::io::Result<()> {
        Err(Self::read_only(path))
    }

    fn sync_directory(&self) -> std::io::Result<()> {
        Ok(())
    }

    /// A reader may take the meta lock; nothing is written here, so a lock held in memory only is enough.
    fn acquire_lock(&self, _lock: &Lock) -> Result<DirectoryLock, LockError> {
        Ok(DirectoryLock::from(Box::new(())))
    }

    fn watch(&self, _watch_callback: WatchCallback) -> ld_lucivy::Result<WatchHandle> {
        Ok(WatchHandle::empty())
    }
}
