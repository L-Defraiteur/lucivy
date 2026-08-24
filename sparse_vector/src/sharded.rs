//! `ShardedSparseHandle`: N [`SparseHandle`]s behind a [`ShardRouter`] and a
//! pool of luciole actors — the sparse counterpart of `lucivy_core`'s
//! `ShardedHandle`.
//!
//! A dot product is local to a shard: no global statistics are needed to
//! merge results, so a search is a scatter of the same `(query, limit,
//! filter)` to every shard and a k-way merge of `(id, score)` by score.
//!
//! Storage is abstracted by [`SparseShardStorage`]: a filesystem layout
//! (`{base}/shard_{i}` plus root files) or a blob store (namespaces
//! `Sparse_{name}/shard_{i}`, root files under `Sparse_{name}`), both built on
//! lucistore's shard storages.
//!
//! Lifecycle contract, identical to the FTS handle: `commit()` persists every
//! shard and the router; `close()` commits, stops the actors and makes the
//! handle inert (every entry point answers `"handle is closed"`);
//! `drop_index()` closes and destroys the storage.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use lucistore::blob_store::BlobStore;
use lucistore::shard_router::ShardRouter;
use lucistore::shard_storage::{
    BlobShardStorage, FsShardStorage, ShardStorage as RootStorage,
};
use luciole::{Actor, ActorStatus, Pool, Priority, Reply};
use serde::{Deserialize, Serialize};

use crate::handle::SparseHandle;
use crate::index::SparseVector;

const CONFIG_FILE: &str = "_sparse_config.json";
const ROUTER_FILE: &str = "_sparse_router.bin";
/// Blob namespace prefix, shared with `SparseHandle` so one store can hold
/// FTS (`Lucivy_`) and sparse (`Sparse_`) indexes of the same name.
const BLOB_PREFIX: &str = "Sparse_";

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Persisted as `_sparse_config.json` at the root of the storage.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShardedSparseConfig {
    /// Number of shards, at least 1.
    pub shards: usize,
    /// Routing balance, 0.0..=1.0: 1.0 is round-robin (the default), lower
    /// values co-locate vectors sharing dimensions on the same shard.
    #[serde(default = "default_balance_weight")]
    pub balance_weight: f64,
    /// Dimensions with a global document frequency above this are not
    /// tracked for routing (they carry no locality information).
    #[serde(default = "default_df_threshold")]
    pub df_threshold: u32,
}

fn default_balance_weight() -> f64 {
    1.0
}

fn default_df_threshold() -> u32 {
    5000
}

impl ShardedSparseConfig {
    pub fn new(shards: usize) -> Self {
        Self {
            shards,
            balance_weight: default_balance_weight(),
            df_threshold: default_df_threshold(),
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.shards == 0 {
            return Err("'shards' must be at least 1".into());
        }
        if !(0.0..=1.0).contains(&self.balance_weight) {
            return Err(format!(
                "'balance_weight' must be within 0.0..=1.0, got {}",
                self.balance_weight
            ));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

/// Where the shards and the root files of a sharded sparse index live.
pub trait SparseShardStorage: Send + Sync {
    fn create_shard(&self, shard_id: usize) -> Result<SparseHandle, String>;
    fn open_shard(&self, shard_id: usize) -> Result<SparseHandle, String>;
    fn write_root_file(&self, name: &str, data: &[u8]) -> Result<(), String>;
    fn read_root_file(&self, name: &str) -> Result<Vec<u8>, String>;
    fn root_file_exists(&self, name: &str) -> bool;
    /// Destroy everything held for the index. Called by
    /// [`ShardedSparseHandle::drop_index`] once the handle is closed.
    fn drop_storage(&self, _num_shards: usize) -> Result<(), String> {
        Err("dropping this storage backend is not supported".into())
    }
}

/// Filesystem storage: `{base}/shard_{i}/` per shard, root files in `{base}`.
pub struct FsSparseStorage {
    inner: FsShardStorage,
}

impl FsSparseStorage {
    pub fn new(base_path: impl Into<PathBuf>) -> Result<Self, String> {
        Ok(Self {
            inner: FsShardStorage::new(base_path)?,
        })
    }

    pub fn base_path(&self) -> &Path {
        self.inner.base_path()
    }
}

impl SparseShardStorage for FsSparseStorage {
    fn create_shard(&self, shard_id: usize) -> Result<SparseHandle, String> {
        let path = self.inner.shard_path(shard_id);
        SparseHandle::create(&path.to_string_lossy())
    }

    fn open_shard(&self, shard_id: usize) -> Result<SparseHandle, String> {
        let path = self.inner.shard_path(shard_id);
        SparseHandle::open(&path.to_string_lossy())
    }

    fn write_root_file(&self, name: &str, data: &[u8]) -> Result<(), String> {
        self.inner.write_root_file(name, data)
    }

    fn read_root_file(&self, name: &str) -> Result<Vec<u8>, String> {
        self.inner.read_root_file(name)
    }

    fn root_file_exists(&self, name: &str) -> bool {
        self.inner.root_file_exists(name)
    }

    fn drop_storage(&self, _num_shards: usize) -> Result<(), String> {
        std::fs::remove_dir_all(self.inner.base_path())
            .map_err(|e| format!("cannot remove {}: {e}", self.inner.base_path().display()))
    }
}

/// Blob storage: shard `i` is the `SparseHandle` namespace
/// `Sparse_{name}/shard_{i}`, root files live under `Sparse_{name}`. The
/// local cache under `cache_base` is disposable; the store is the truth.
pub struct BlobSparseStorage<S: BlobStore> {
    store: Arc<S>,
    inner: BlobShardStorage<S>,
    name: String,
    cache_base: PathBuf,
}

impl<S: BlobStore> BlobSparseStorage<S> {
    pub fn new(store: Arc<S>, name: impl Into<String>, cache_base: impl Into<PathBuf>) -> Self {
        let name = name.into();
        let cache_base = cache_base.into();
        let inner = BlobShardStorage::new(
            store.clone(),
            format!("{BLOB_PREFIX}{name}"),
            Some(cache_base.clone()),
        );
        Self {
            store,
            inner,
            name,
            cache_base,
        }
    }

    fn shard_index_name(&self, shard_id: usize) -> String {
        format!("{}/shard_{shard_id}", self.name)
    }
}

impl<S: BlobStore> SparseShardStorage for BlobSparseStorage<S> {
    fn create_shard(&self, shard_id: usize) -> Result<SparseHandle, String> {
        let store: Arc<dyn BlobStore> = self.store.clone();
        SparseHandle::create_with_store(store, &self.shard_index_name(shard_id), &self.cache_base)
    }

    fn open_shard(&self, shard_id: usize) -> Result<SparseHandle, String> {
        let store: Arc<dyn BlobStore> = self.store.clone();
        SparseHandle::open_with_store(store, &self.shard_index_name(shard_id), &self.cache_base)
    }

    fn write_root_file(&self, name: &str, data: &[u8]) -> Result<(), String> {
        self.inner.write_root_file(name, data)
    }

    fn read_root_file(&self, name: &str) -> Result<Vec<u8>, String> {
        self.inner.read_root_file(name)
    }

    fn root_file_exists(&self, name: &str) -> bool {
        self.inner.root_file_exists(name)
    }

    fn drop_storage(&self, num_shards: usize) -> Result<(), String> {
        let mut namespaces: Vec<String> = (0..num_shards)
            .map(|i| format!("{BLOB_PREFIX}{}", self.shard_index_name(i)))
            .collect();
        namespaces.push(format!("{BLOB_PREFIX}{}", self.name));
        for ns in namespaces {
            let files = self
                .store
                .list(&ns)
                .map_err(|e| format!("cannot list {ns}: {e}"))?;
            for f in files {
                self.store
                    .delete(&ns, &f)
                    .map_err(|e| format!("cannot delete {ns}/{f}: {e}"))?;
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Shard actor
// ---------------------------------------------------------------------------

enum SparseShardMsg {
    Insert {
        node_id: u64,
        vector: SparseVector,
    },
    Remove {
        node_id: u64,
        reply: Reply<Result<bool, String>>,
    },
    Search {
        query: Arc<SparseVector>,
        limit: usize,
        filter: Option<Arc<Vec<u64>>>,
        reply: Reply<Vec<(u64, f32)>>,
    },
    Commit {
        reply: Reply<Result<(), String>>,
    },
    Drain(luciole::DrainMsg),
    Shutdown(luciole::ShutdownMsg),
}

impl From<luciole::DrainMsg> for SparseShardMsg {
    fn from(d: luciole::DrainMsg) -> Self {
        SparseShardMsg::Drain(d)
    }
}

impl From<luciole::ShutdownMsg> for SparseShardMsg {
    fn from(s: luciole::ShutdownMsg) -> Self {
        SparseShardMsg::Shutdown(s)
    }
}

struct SparseShardActor {
    shard_id: usize,
    handle: Arc<SparseHandle>,
    /// First insert error since the last commit, reported by `commit()`:
    /// inserts are fire-and-forget, so their failures surface there.
    pending_error: Option<String>,
}

impl Actor for SparseShardActor {
    type Msg = SparseShardMsg;

    fn name(&self) -> &'static str {
        "sparse_shard"
    }

    fn priority(&self) -> Priority {
        Priority::Medium
    }

    fn handle(&mut self, msg: SparseShardMsg, ctx: &luciole::ActorContext) -> ActorStatus {
        match msg {
            SparseShardMsg::Insert { node_id, vector } => {
                if let Err(e) = self.handle.insert(node_id, &vector) {
                    self.pending_error
                        .get_or_insert_with(|| format!("shard_{}: insert {node_id}: {e}", self.shard_id));
                }
            }
            SparseShardMsg::Remove { node_id, reply } => {
                reply.send(self.handle.remove(node_id));
            }
            SparseShardMsg::Search { query, limit, filter, reply } => {
                ctx.set_activity(format!("search sparse_shard_{}", self.shard_id));
                let hits = match filter {
                    Some(ids) => self.handle.search_filtered(&query, limit, &ids),
                    None => self.handle.search(&query, limit),
                };
                reply.send(hits);
            }
            SparseShardMsg::Commit { reply } => {
                ctx.set_activity(format!("commit sparse_shard_{}", self.shard_id));
                let result = match self.pending_error.take() {
                    Some(e) => Err(e),
                    None => self
                        .handle
                        .commit_inner()
                        .map_err(|e| format!("shard_{}: commit: {e}", self.shard_id)),
                };
                reply.send(result);
            }
            SparseShardMsg::Drain(d) => d.ack(),
            SparseShardMsg::Shutdown(s) => {
                s.ack();
                return ActorStatus::Stop;
            }
        }
        ActorStatus::Continue
    }
}

// ---------------------------------------------------------------------------
// Handle
// ---------------------------------------------------------------------------

pub struct ShardedSparseHandle {
    storage: Box<dyn SparseShardStorage>,
    config: ShardedSparseConfig,
    shards: Vec<Arc<SparseHandle>>,
    router: Mutex<ShardRouter>,
    pool: Pool<SparseShardMsg>,
    closed: AtomicBool,
}

impl ShardedSparseHandle {
    // ── Construction ────────────────────────────────────────────────────

    /// Create a new sharded index on a filesystem directory.
    pub fn create(base_path: &str, config: &ShardedSparseConfig) -> Result<Self, String> {
        Self::create_with_storage(Box::new(FsSparseStorage::new(base_path)?), config)
    }

    /// Open an existing sharded index from a filesystem directory.
    pub fn open(base_path: &str) -> Result<Self, String> {
        Self::open_with_storage(Box::new(FsSparseStorage::new(base_path)?))
    }

    /// Create a new sharded index whose truth is a blob store; `cache_base`
    /// holds the disposable local mmap cache.
    pub fn create_with_store<S: BlobStore>(
        store: Arc<S>,
        name: &str,
        cache_base: &Path,
        config: &ShardedSparseConfig,
    ) -> Result<Self, String> {
        Self::create_with_storage(Box::new(BlobSparseStorage::new(store, name, cache_base)), config)
    }

    /// Open an existing sharded index from a blob store.
    pub fn open_with_store<S: BlobStore>(
        store: Arc<S>,
        name: &str,
        cache_base: &Path,
    ) -> Result<Self, String> {
        Self::open_with_storage(Box::new(BlobSparseStorage::new(store, name, cache_base)))
    }

    pub fn create_with_storage(
        storage: Box<dyn SparseShardStorage>,
        config: &ShardedSparseConfig,
    ) -> Result<Self, String> {
        config.validate()?;
        if storage.root_file_exists(CONFIG_FILE) {
            return Err(format!("a sharded sparse index already exists here ({CONFIG_FILE} present)"));
        }
        let config_json = serde_json::to_vec_pretty(config)
            .map_err(|e| format!("cannot serialize config: {e}"))?;
        storage.write_root_file(CONFIG_FILE, &config_json)?;

        let mut shards = Vec::with_capacity(config.shards);
        for i in 0..config.shards {
            shards.push(Arc::new(storage.create_shard(i)?));
        }
        let router = ShardRouter::with_options(config.shards, config.df_threshold, config.balance_weight);
        storage.write_root_file(ROUTER_FILE, &router.to_bytes())?;

        Ok(Self::assemble(storage, config.clone(), shards, router))
    }

    pub fn open_with_storage(storage: Box<dyn SparseShardStorage>) -> Result<Self, String> {
        let config_json = storage.read_root_file(CONFIG_FILE)?;
        let config: ShardedSparseConfig = serde_json::from_slice(&config_json)
            .map_err(|e| format!("invalid {CONFIG_FILE}: {e}"))?;
        config.validate()?;

        let mut shards = Vec::with_capacity(config.shards);
        for i in 0..config.shards {
            shards.push(Arc::new(storage.open_shard(i)?));
        }
        let router = if storage.root_file_exists(ROUTER_FILE) {
            ShardRouter::from_bytes(&storage.read_root_file(ROUTER_FILE)?)?
        } else {
            ShardRouter::with_options(config.shards, config.df_threshold, config.balance_weight)
        };
        Ok(Self::assemble(storage, config, shards, router))
    }

    fn assemble(
        storage: Box<dyn SparseShardStorage>,
        config: ShardedSparseConfig,
        shards: Vec<Arc<SparseHandle>>,
        router: ShardRouter,
    ) -> Self {
        let handles = shards.clone();
        // Capacity 0 = unbounded mailbox: inserts are fire-and-forget.
        let pool = Pool::spawn(shards.len(), 0, |i| SparseShardActor {
            shard_id: i,
            handle: Arc::clone(&handles[i]),
            pending_error: None,
        });
        Self {
            storage,
            config,
            shards,
            router: Mutex::new(router),
            pool,
            closed: AtomicBool::new(false),
        }
    }

    // ── Introspection ───────────────────────────────────────────────────

    pub fn num_shards(&self) -> usize {
        self.shards.len()
    }

    pub fn config(&self) -> &ShardedSparseConfig {
        &self.config
    }

    /// Number of vectors across shards, as of the last commit of each.
    pub fn len(&self) -> usize {
        self.shards.iter().map(|s| s.len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Shard holding `node_id`, if the router saw it inserted.
    pub fn shard_for_node_id(&self, node_id: u64) -> Option<usize> {
        self.router.lock().ok()?.shard_for_node_id(node_id)
    }

    fn ensure_open(&self) -> Result<(), String> {
        if self.closed.load(Ordering::Acquire) {
            Err("handle is closed".to_string())
        } else {
            Ok(())
        }
    }

    // ── Writes ──────────────────────────────────────────────────────────

    /// Route the vector to a shard by its dimensions and queue the insert.
    /// Failures inside the shard surface at the next `commit()`.
    pub fn insert(&self, node_id: u64, vector: &SparseVector) -> Result<(), String> {
        self.ensure_open()?;
        let hashes: Vec<u64> = vector
            .indices
            .iter()
            .map(|d| ShardRouter::hash_bytes(&d.to_le_bytes()))
            .collect();
        let shard_id = {
            let mut router = self.router.lock().map_err(|_| "router lock poisoned")?;
            if let Some(known) = router.shard_for_node_id(node_id) {
                // Re-inserting an id keeps it on its shard: the upsert
                // replaces the vector there instead of duplicating it.
                known
            } else {
                let sid = router.route(&hashes);
                router.record_node_id(node_id, sid);
                sid
            }
        };
        self.pool
            .send_to(shard_id, SparseShardMsg::Insert { node_id, vector: vector.clone() })
            .map_err(|e| format!("shard_{shard_id}: {e}"))
    }

    /// Remove a vector. Returns whether it existed.
    pub fn remove(&self, node_id: u64) -> Result<bool, String> {
        self.ensure_open()?;
        let known = self
            .router
            .lock()
            .map_err(|_| "router lock poisoned")?
            .remove_node_id(node_id);
        match known {
            Some(sid) => self
                .pool
                .request_to(sid, |r| SparseShardMsg::Remove { node_id, reply: r }, "sparse_remove")?,
            // Unknown to the router (index built before routing was
            // persisted, or router lost): ask every shard.
            None => {
                let results = self
                    .pool
                    .scatter(|r| SparseShardMsg::Remove { node_id, reply: r }, "sparse_remove_all");
                let mut removed = false;
                for r in results {
                    removed |= r?;
                }
                Ok(removed)
            }
        }
    }

    /// Persist every shard and the router. Reports the first insert error
    /// of each shard since its last commit.
    pub fn commit(&self) -> Result<(), String> {
        self.ensure_open()?;
        self.pool.drain("sparse_drain");
        let results = self
            .pool
            .scatter(|r| SparseShardMsg::Commit { reply: r }, "sparse_commit");
        for r in results {
            r?;
        }
        let router = self.router.lock().map_err(|_| "router lock poisoned")?;
        self.storage.write_root_file(ROUTER_FILE, &router.to_bytes())
    }

    // ── Reads ───────────────────────────────────────────────────────────

    /// Top-`limit` by dot product across all shards.
    pub fn search(&self, query: &SparseVector, limit: usize) -> Result<Vec<(u64, f32)>, String> {
        self.search_inner(query, limit, None)
    }

    /// `search` restricted to `allowed_ids`.
    pub fn search_filtered(
        &self,
        query: &SparseVector,
        limit: usize,
        allowed_ids: &[u64],
    ) -> Result<Vec<(u64, f32)>, String> {
        self.ensure_open()?;
        if allowed_ids.is_empty() || limit == 0 || query.indices.is_empty() {
            return Ok(Vec::new());
        }
        // The router knows where every inserted id lives: give each shard
        // only its share and leave the others idle. An id the router never
        // saw (an index older than routing persistence) sends the whole set
        // everywhere, as before.
        let per_shard: Option<Vec<Vec<u64>>> = {
            let router = self.router.lock().map_err(|_| "router lock poisoned")?;
            let mut groups: Vec<Vec<u64>> = vec![Vec::new(); self.shards.len()];
            let mut all_known = true;
            for &id in allowed_ids {
                match router.shard_for_node_id(id) {
                    Some(sid) if sid < groups.len() => groups[sid].push(id),
                    _ => {
                        all_known = false;
                        break;
                    }
                }
            }
            all_known.then_some(groups)
        };
        let Some(groups) = per_shard else {
            return self.search_inner(query, limit, Some(Arc::new(allowed_ids.to_vec())));
        };
        let targets: Vec<usize> = (0..groups.len()).filter(|&i| !groups[i].is_empty()).collect();
        if targets.is_empty() {
            return Ok(Vec::new());
        }
        let query = Arc::new(query.clone());
        let shares: Vec<Option<Arc<Vec<u64>>>> = groups
            .into_iter()
            .map(|g| (!g.is_empty()).then(|| Arc::new(g)))
            .collect();
        let per_shard = self.pool.scatter_to(
            &targets,
            |sid, r| SparseShardMsg::Search {
                query: Arc::clone(&query),
                limit,
                filter: shares[sid].clone(),
                reply: r,
            },
            "sparse_search_routed",
        );
        Ok(merge_top_k(per_shard.into_iter().map(|(_, hits)| hits).collect(), limit))
    }

    fn search_inner(
        &self,
        query: &SparseVector,
        limit: usize,
        filter: Option<Arc<Vec<u64>>>,
    ) -> Result<Vec<(u64, f32)>, String> {
        self.ensure_open()?;
        if limit == 0 || query.indices.is_empty() {
            return Ok(Vec::new());
        }
        let query = Arc::new(query.clone());
        let per_shard = self.pool.scatter(
            |r| SparseShardMsg::Search {
                query: Arc::clone(&query),
                limit,
                filter: filter.clone(),
                reply: r,
            },
            "sparse_search",
        );
        Ok(merge_top_k(per_shard, limit))
    }

    // ── Lifecycle ───────────────────────────────────────────────────────

    /// Commit, stop the shard actors, make the handle inert.
    pub fn close(&self) -> Result<(), String> {
        if self.closed.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        self.pool.drain("sparse_close_drain");
        let results = self
            .pool
            .scatter(|r| SparseShardMsg::Commit { reply: r }, "sparse_close_commit");
        let mut first_err = None;
        for r in results {
            if let Err(e) = r {
                first_err.get_or_insert(e);
            }
        }
        if let Ok(router) = self.router.lock() {
            if let Err(e) = self.storage.write_root_file(ROUTER_FILE, &router.to_bytes()) {
                first_err.get_or_insert(e);
            }
        }
        self.pool.shutdown("sparse_close_shards");
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Close, then destroy the storage (shards and root files).
    pub fn drop_index(self) -> Result<(), String> {
        self.close()?;
        let n = self.shards.len();
        // The shard handles hold the local caches; release them (and the
        // actor pool that shares them) before the storage removes what
        // they point to.
        let Self {
            storage,
            shards,
            pool,
            ..
        } = self;
        drop(pool);
        drop(shards);
        storage.drop_storage(n)
    }
}

/// Merge per-shard top lists: score descending, then id ascending, at most
/// `limit` entries.
fn merge_top_k(per_shard: Vec<Vec<(u64, f32)>>, limit: usize) -> Vec<(u64, f32)> {
    let mut all: Vec<(u64, f32)> = per_shard.into_iter().flatten().collect();
    all.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });
    all.truncate(limit);
    all
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blob_store::MemBlobStore;

    fn tmp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("sparse_sharded_{name}"));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    /// Deterministic corpus: 240 vectors over 64 dims, 6-12 non-zeros each,
    /// with a few hub dimensions so some posting lists are long.
    fn corpus() -> Vec<(u64, SparseVector)> {
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        (0..240u64)
            .map(|id| {
                let n = 6 + (next() % 7) as usize;
                let mut indices: Vec<u32> = Vec::new();
                while indices.len() < n {
                    let d = if next() % 3 == 0 { (next() % 4) as u32 } else { (next() % 64) as u32 };
                    if !indices.contains(&d) {
                        indices.push(d);
                    }
                }
                indices.sort_unstable();
                let values: Vec<f32> = indices
                    .iter()
                    .map(|_| ((next() % 1000) as f32 / 100.0) - 2.0)
                    .collect();
                (id, SparseVector::new(indices, values))
            })
            .collect()
    }

    fn queries() -> Vec<SparseVector> {
        vec![
            SparseVector::new(vec![0, 1, 2], vec![1.0, 0.5, 0.25]),
            SparseVector::new(vec![3, 17, 40, 63], vec![2.0, 1.0, 1.0, 0.5]),
            SparseVector::new(vec![1, 9], vec![-1.0, 3.0]),
            SparseVector::new(vec![50], vec![1.0]),
        ]
    }

    /// Single, unsharded handle over `docs`; `name` keeps concurrent tests
    /// out of each other's directory.
    fn reference(docs: &[(u64, SparseVector)], name: &str) -> SparseHandle {
        let h = SparseHandle::create(&tmp(name).to_string_lossy()).unwrap();
        for (id, v) in docs {
            h.insert(*id, v).unwrap();
        }
        h.commit_inner().unwrap();
        h
    }

    fn assert_same(label: &str, got: &[(u64, f32)], want: &[(u64, f32)]) {
        assert_eq!(got.len(), want.len(), "{label}: {got:?} vs {want:?}");
        for (g, w) in got.iter().zip(want) {
            assert_eq!(g.0, w.0, "{label}: ids differ: {got:?} vs {want:?}");
            assert!((g.1 - w.1).abs() < 1e-4, "{label}: scores differ: {got:?} vs {want:?}");
        }
    }

    #[test]
    fn sharded_equals_single_handle_fs() {
        let docs = corpus();
        let single = reference(&docs, "reference_fs4");
        let base = tmp("fs4");
        let h = ShardedSparseHandle::create(&base.to_string_lossy(), &ShardedSparseConfig::new(4)).unwrap();
        for (id, v) in &docs {
            h.insert(*id, v).unwrap();
        }
        h.commit().unwrap();
        assert_eq!(h.len(), docs.len());
        // Every shard got something.
        let counts: Vec<usize> = h.shards.iter().map(|s| s.len()).collect();
        assert!(counts.iter().all(|&c| c > 0), "{counts:?}");

        for (i, q) in queries().iter().enumerate() {
            let got = h.search(q, 10).unwrap();
            let want = single.search(q, 10);
            assert_same(&format!("query {i}"), &got, &want);
        }
        let allowed: Vec<u64> = (0..240).filter(|id| id % 3 == 0).collect();
        for (i, q) in queries().iter().enumerate() {
            let got = h.search_filtered(q, 10, &allowed).unwrap();
            let want = single.search_filtered(q, 10, &allowed);
            assert_same(&format!("filtered query {i}"), &got, &want);
            assert!(got.iter().all(|(id, _)| id % 3 == 0));
        }
        h.close().unwrap();
    }

    #[test]
    fn sharded_remove_reopen_and_closed_refusal() {
        let docs = corpus();
        let base = tmp("fs_reopen");
        let q = &queries()[0];
        let before;
        {
            let h = ShardedSparseHandle::create(&base.to_string_lossy(), &ShardedSparseConfig::new(3)).unwrap();
            for (id, v) in &docs {
                h.insert(*id, v).unwrap();
            }
            h.commit().unwrap();
            let top = h.search(q, 5).unwrap();
            let victim = top[0].0;
            assert!(h.shard_for_node_id(victim).is_some());
            assert!(h.remove(victim).unwrap());
            assert!(!h.remove(victim).unwrap(), "second remove finds nothing");
            h.commit().unwrap();
            before = h.search(q, 5).unwrap();
            assert!(before.iter().all(|(id, _)| *id != victim));
            h.close().unwrap();
            assert!(h.search(q, 5).unwrap_err().contains("closed"));
            assert!(h.insert(999, &docs[0].1).unwrap_err().contains("closed"));
            assert!(h.commit().unwrap_err().contains("closed"));
        }
        let h = ShardedSparseHandle::open(&base.to_string_lossy()).unwrap();
        assert_eq!(h.num_shards(), 3);
        assert_eq!(h.len(), docs.len() - 1);
        let after = h.search(q, 5).unwrap();
        assert_same("after reopen", &after, &before);
        // The router came back: a known id is removed from its own shard.
        let id = after[0].0;
        assert!(h.shard_for_node_id(id).is_some());
        assert!(h.remove(id).unwrap());
        h.close().unwrap();
    }

    #[test]
    fn sharded_blob_store_is_the_truth() {
        let docs = corpus();
        let single = reference(&docs, "reference_blob");
        let store = Arc::new(MemBlobStore::new());
        let cache_a = tmp("blob_cache_a");
        let cache_b = tmp("blob_cache_b");
        let cfg = ShardedSparseConfig::new(2);
        {
            let h = ShardedSparseHandle::create_with_store(store.clone(), "vectors", &cache_a, &cfg).unwrap();
            for (id, v) in &docs {
                h.insert(*id, v).unwrap();
            }
            h.commit().unwrap();
            h.close().unwrap();
        }
        // Another machine: fresh cache, same store.
        let _ = std::fs::remove_dir_all(&cache_a);
        let h = ShardedSparseHandle::open_with_store(store.clone(), "vectors", &cache_b).unwrap();
        assert_eq!(h.len(), docs.len());
        for (i, q) in queries().iter().enumerate() {
            let got = h.search(q, 10).unwrap();
            let want = single.search(q, 10);
            assert_same(&format!("blob query {i}"), &got, &want);
        }
        // drop_index leaves nothing in the store.
        h.drop_index().unwrap();
        for ns in ["Sparse_vectors", "Sparse_vectors/shard_0", "Sparse_vectors/shard_1"] {
            assert!(store.list(ns).unwrap().is_empty(), "{ns} not empty");
        }
    }

    /// `search_filtered` picks a seek path for small allowed sets and a
    /// window path for large ones, per shard; every combination must give
    /// the single-handle answer, and the answer must be the unfiltered
    /// ranking restricted to the allowed ids.
    #[test]
    fn filtered_search_agrees_across_paths_and_sizes() {
        let docs = corpus();
        let single = reference(&docs, "paths");
        let base = tmp("fs_paths");
        let h = ShardedSparseHandle::create(&base.to_string_lossy(), &ShardedSparseConfig::new(4)).unwrap();
        for (id, v) in &docs {
            h.insert(*id, v).unwrap();
        }
        h.commit().unwrap();
        for (qi, q) in queries().iter().enumerate() {
            let full = h.search(q, 240).unwrap();
            for size in [1usize, 2, 5, 17, 60, 240] {
                let allowed: Vec<u64> = (0..240u64).filter(|id| (id * 7 + qi as u64) % 240 < size as u64).collect();
                let got = h.search_filtered(q, 10, &allowed).unwrap();
                let want = single.search_filtered(q, 10, &allowed);
                assert_same(&format!("q{qi} size {size}"), &got, &want);
                let expect: Vec<(u64, f32)> = full
                    .iter()
                    .filter(|(id, _)| allowed.contains(id))
                    .take(10)
                    .copied()
                    .collect();
                assert_same(&format!("q{qi} size {size} vs unfiltered"), &got, &expect);
            }
        }
        h.close().unwrap();
    }

    #[test]
    fn config_is_checked() {
        let base = tmp("bad_config");
        let mut cfg = ShardedSparseConfig::new(0);
        let err = ShardedSparseHandle::create(&base.to_string_lossy(), &cfg).err().unwrap();
        assert!(err.contains("shards"), "{err}");
        cfg.shards = 2;
        cfg.balance_weight = 3.0;
        let err = ShardedSparseHandle::create(&base.to_string_lossy(), &cfg).err().unwrap();
        assert!(err.contains("balance_weight"), "{err}");
        let bad: Result<ShardedSparseConfig, _> = serde_json::from_str(r#"{"shards": 2, "shard": 4}"#);
        assert!(bad.is_err(), "unknown keys must be refused");
    }
}
