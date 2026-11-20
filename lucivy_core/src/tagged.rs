//! **Tagged generations** (lucivy 4.4, the B way of `docs/lucivy/20-11-2026/02-conception-lucivy-4-4-la-voie-b.md` in
//! rag3db). A caller whose database names a generation of the index tags each commit with it, keeps on disk the last
//! generation its database validated and the ones above, and can bring the four shards back to one of them, or open
//! one for reading without touching the directory. Nothing here is implemented yet: every call says so.

use crate::sharded_handle::ShardedHandle;

const NOT_YET: &str = "tagged generations: not implemented yet (lucivy 4.4.0)";

impl ShardedHandle {
    /// Commits every shard as generation `g`, strictly above the last tagged one. The tagged generations from the
    /// last released one up stay on disk until `release_below`.
    pub fn commit_tagged(&self, _g: u64) -> Result<(), String> {
        Err(NOT_YET.into())
    }

    /// The caller's database has durably validated `g`: every tagged generation below `g` may go; their files leave
    /// at the next garbage collection. Never `g` itself, nor anything above.
    pub fn release_below(&self, _g: u64) -> Result<(), String> {
        Err(NOT_YET.into())
    }

    /// Brings the four shards back to generation `g`, kept on disk, all or nothing; the generations above are
    /// forgotten.
    pub fn revert_to(&self, _g: u64) -> Result<(), String> {
        Err(NOT_YET.into())
    }

    /// The tagged generations on disk, ascending, and the current one.
    pub fn tagged_generations(&self) -> Result<(Vec<u64>, u64), String> {
        Err(NOT_YET.into())
    }

    /// Opens generation `g` of a directory for reading only: nothing written, nothing deleted.
    pub fn open_read_only_at(_path: &str, _g: u64) -> Result<ShardedHandle, String> {
        Err(NOT_YET.into())
    }
}
