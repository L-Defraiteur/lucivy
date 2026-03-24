//! SyncServer — server-side version history and delta dispatch.
//!
//! Tracks the last N versions per shard so it can compute deltas for clients
//! that are slightly behind. If a client's version is too old (not in history),
//! the server signals a full snapshot is needed.
//!
//! Engine-agnostic: works with version strings and segment ID sets.

use std::collections::{HashSet, VecDeque};

use crate::delta::IndexDelta;
use crate::delta_sharded::ShardedDelta;

/// A version snapshot: version string + the bundle IDs at that point.
#[derive(Debug, Clone)]
struct VersionEntry {
    version: String,
    segment_ids: HashSet<String>,
}

/// What the server returns to a single-shard sync request.
pub enum SyncResponse {
    /// Client's segment IDs for the requested version (caller uses these to compute delta).
    ClientSegments {
        version: String,
        segment_ids: HashSet<String>,
    },
    /// Client version too old or unknown — needs a full snapshot.
    FullSnapshot,
    /// Shard is already up-to-date.
    UpToDate,
}

/// What the server returns for a sharded sync request.
pub enum ShardedSyncResponse {
    /// Per-shard responses. Only contains shards that need action.
    Responses(Vec<(usize, SyncResponse)>),
    /// All requested shards are up-to-date.
    UpToDate,
}

/// Server-side sync helper that tracks version history.
pub struct SyncServer {
    shard_histories: Vec<VecDeque<VersionEntry>>,
    max_history: usize,
    num_shards: usize,
}

impl SyncServer {
    /// Create a new SyncServer for a single-shard index.
    pub fn new(max_history: usize) -> Self {
        Self {
            shard_histories: vec![VecDeque::with_capacity(max_history)],
            max_history,
            num_shards: 1,
        }
    }

    /// Create a new SyncServer for a sharded index.
    pub fn new_sharded(num_shards: usize, max_history: usize) -> Self {
        let mut shard_histories = Vec::with_capacity(num_shards);
        for _ in 0..num_shards {
            shard_histories.push(VecDeque::with_capacity(max_history));
        }
        Self {
            shard_histories,
            max_history,
            num_shards,
        }
    }

    /// Number of shards this server tracks.
    pub fn num_shards(&self) -> usize {
        self.num_shards
    }

    /// Record a new version for a shard.
    ///
    /// Call this after each commit. The caller computes the version and segment IDs
    /// from the engine's manifest (e.g. via `compute_version_from_bytes` + `segment_ids_from_meta`).
    pub fn record_version(
        &mut self,
        shard_id: usize,
        version: String,
        segment_ids: HashSet<String>,
    ) -> Result<(), String> {
        if shard_id >= self.num_shards {
            return Err(format!("shard_id {shard_id} >= num_shards {}", self.num_shards));
        }

        let history = &mut self.shard_histories[shard_id];

        // Don't record duplicate consecutive versions.
        if history.back().map(|e| &e.version) == Some(&version) {
            return Ok(());
        }

        if history.len() >= self.max_history {
            history.pop_front();
        }
        history.push_back(VersionEntry { version, segment_ids });

        Ok(())
    }

    /// Convenience: record version for shard 0 (single-shard index).
    pub fn record(&mut self, version: String, segment_ids: HashSet<String>) -> Result<(), String> {
        self.record_version(0, version, segment_ids)
    }

    /// Current version of a shard (or shard 0 for single-shard).
    pub fn current_version(&self, shard_id: usize) -> Option<&str> {
        self.shard_histories
            .get(shard_id)
            .and_then(|h| h.back())
            .map(|e| e.version.as_str())
    }

    /// Look up a client's version in a shard's history.
    ///
    /// Returns the client's segment IDs if found, or signals FullSnapshot/UpToDate.
    pub fn lookup(&self, shard_id: usize, client_version: &str) -> Result<SyncResponse, String> {
        if shard_id >= self.num_shards {
            return Err(format!("shard_id {shard_id} >= num_shards {}", self.num_shards));
        }

        let history = &self.shard_histories[shard_id];

        // Check if client is already up-to-date.
        if let Some(latest) = history.back() {
            if latest.version == client_version {
                return Ok(SyncResponse::UpToDate);
            }
        }

        // Find client's version in history.
        match history.iter().find(|e| e.version == client_version) {
            Some(entry) => Ok(SyncResponse::ClientSegments {
                version: entry.version.clone(),
                segment_ids: entry.segment_ids.clone(),
            }),
            None => Ok(SyncResponse::FullSnapshot),
        }
    }

    /// Convenience: lookup for shard 0 (single-shard index).
    pub fn lookup_single(&self, client_version: &str) -> Result<SyncResponse, String> {
        self.lookup(0, client_version)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_record_and_lookup() {
        let mut server = SyncServer::new(10);
        let ids_v1: HashSet<String> = ["seg1".into(), "seg2".into()].into();
        server.record("v1".into(), ids_v1.clone()).unwrap();

        // Up-to-date.
        assert!(matches!(server.lookup_single("v1").unwrap(), SyncResponse::UpToDate));

        // Record v2.
        let ids_v2: HashSet<String> = ["seg1".into(), "seg3".into()].into();
        server.record("v2".into(), ids_v2).unwrap();

        // v1 client gets their segment IDs back.
        match server.lookup_single("v1").unwrap() {
            SyncResponse::ClientSegments { segment_ids, .. } => {
                assert_eq!(segment_ids, ids_v1);
            }
            _ => panic!("expected ClientSegments"),
        }

        // Unknown version.
        assert!(matches!(server.lookup_single("unknown").unwrap(), SyncResponse::FullSnapshot));
    }

    #[test]
    fn test_history_overflow() {
        let mut server = SyncServer::new(3);
        for i in 0..5 {
            server.record(format!("v{i}"), HashSet::new()).unwrap();
        }
        // v0, v1 evicted.
        assert!(matches!(server.lookup_single("v0").unwrap(), SyncResponse::FullSnapshot));
        assert!(matches!(server.lookup_single("v1").unwrap(), SyncResponse::FullSnapshot));
        // v2 still there.
        assert!(!matches!(server.lookup_single("v2").unwrap(), SyncResponse::FullSnapshot));
    }

    #[test]
    fn test_sharded() {
        let mut server = SyncServer::new_sharded(2, 10);
        server.record_version(0, "v1".into(), HashSet::new()).unwrap();
        server.record_version(1, "v1".into(), HashSet::new()).unwrap();

        assert!(matches!(server.lookup(0, "v1").unwrap(), SyncResponse::UpToDate));
        assert!(matches!(server.lookup(1, "v1").unwrap(), SyncResponse::UpToDate));

        server.record_version(0, "v2".into(), HashSet::new()).unwrap();
        assert!(!matches!(server.lookup(0, "v1").unwrap(), SyncResponse::UpToDate));
        assert!(matches!(server.lookup(1, "v1").unwrap(), SyncResponse::UpToDate));
    }

    #[test]
    fn test_no_duplicate_consecutive() {
        let mut server = SyncServer::new(10);
        server.record("v1".into(), HashSet::new()).unwrap();
        server.record("v1".into(), HashSet::new()).unwrap();
        // Should only have 1 entry, not 2.
        assert_eq!(server.shard_histories[0].len(), 1);
    }
}
