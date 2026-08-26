//! Segments: an index is a list of immutable files plus what is still in RAM.
//!
//! Until 3.0.5 a sparse index was one `sparse.mmap`, rewritten whole at every
//! commit — so inserting one vector cost what inserting the whole index cost
//! (320 ms at 200 000 vectors, growing with the file: see
//! `tests/bench_commit_cost.rs`). A commit now writes **one segment holding
//! the vectors added since the last one**, and `meta.json` lists what is
//! active. The cost of a commit is the cost of the delta.
//!
//! A segment is a version 3 file: its dimension table is keyed by global
//! token id and sorted (see [`crate::mmap_index`]). That is what lets two
//! segments — and therefore two indexes — be merged by walking their tables
//! together, without remapping anything.
//!
//! **Deletions are tombstones.** Removing a document marks its id in the
//! segments that hold it; a segment written afterwards is not concerned,
//! which is what makes an update (delete, then insert) land the right way
//! round. A merge applies them and clears the lists.
//!
//! Knowing *which* segment holds an id is what `seg_<id>.ids` is for: the
//! segment's record ids, sorted, eight bytes each, read only when something
//! is deleted or updated. It replaces the far larger `sparse_vectors.bin`
//! (whole vectors, kept only to know which dimensions to touch on a
//! deletion), and it hands a merge its id list for nothing.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::mmap_index::{write_file_atomic, MmapPostingData};

/// `meta.json` — what this index is made of.
pub const META_FILE: &str = "meta.json";
/// What this build writes; a newer one is refused rather than misread.
pub const META_VERSION: u32 = 1;

/// One segment of an index.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SegmentMeta {
    /// Names its files: `seg_<id>.mmap`.
    pub id: String,
    /// Vectors written into it (before its deletions).
    pub num_vectors: u32,
    /// Ids deleted from it since it was written. Sorted, unique.
    #[serde(default)]
    pub deleted: Vec<u64>,
}

impl SegmentMeta {
    /// Vectors this segment still answers for.
    pub fn live_vectors(&self) -> usize {
        (self.num_vectors as usize).saturating_sub(self.deleted.len())
    }
}

/// The index's manifest: which segments are active, and what is deleted.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IndexMeta {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub segments: Vec<SegmentMeta>,
}

impl Default for IndexMeta {
    fn default() -> Self {
        Self { version: META_VERSION, segments: Vec::new() }
    }
}

impl IndexMeta {
    /// Read `meta.json`, or the default when there is none (an index that
    /// has never been committed, or one from before segments).
    pub fn read(base: &Path) -> Result<Self, String> {
        let path = base.join(META_FILE);
        if !path.exists() {
            return Ok(Self::default());
        }
        let data = std::fs::read(&path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let meta: Self = serde_json::from_slice(&data)
            .map_err(|e| format!("cannot parse {}: {e}", path.display()))?;
        if meta.version > META_VERSION {
            return Err(format!(
                "{} was written by a newer version ({}, this build writes {META_VERSION})",
                path.display(), meta.version,
            ));
        }
        Ok(meta)
    }

    /// Write `meta.json` atomically: a crash leaves the previous manifest,
    /// so the segments it names are still the index.
    pub fn write(&self, base: &Path) -> Result<(), String> {
        let data = serde_json::to_vec_pretty(self)
            .map_err(|e| format!("cannot serialize {META_FILE}: {e}"))?;
        write_file_atomic(&base.join(META_FILE), &data)
    }


    /// Vectors the segments still answer for.
    pub fn live_vectors(&self) -> usize {
        self.segments.iter().map(SegmentMeta::live_vectors).sum()
    }

    /// The files these segments own, for a store that syncs by name.
    pub fn files(&self) -> Vec<String> {
        let mut files: Vec<String> = Vec::with_capacity(self.segments.len() * 2 + 1);
        for s in &self.segments {
            files.push(segment_file(&s.id));
            files.push(ids_file(&s.id));
        }
        files.push(META_FILE.to_string());
        files
    }
}

/// `seg_<id>.mmap`.
pub fn segment_file(id: &str) -> String {
    format!("seg_{id}.mmap")
}

/// `seg_<id>.ids` — the segment's record ids, sorted, little-endian.
pub fn ids_file(id: &str) -> String {
    format!("seg_{id}.ids")
}

/// Serialize sorted ids for [`ids_file`].
pub fn encode_ids(ids: &[u64]) -> Vec<u8> {
    let mut out = Vec::with_capacity(ids.len() * 8);
    for id in ids {
        out.extend_from_slice(&id.to_le_bytes());
    }
    out
}

fn decode_ids(data: &[u8]) -> Result<Vec<u64>, String> {
    if data.len() % 8 != 0 {
        return Err(format!("id list is {} bytes, not a multiple of 8", data.len()));
    }
    Ok(data.chunks_exact(8).map(|c| u64::from_le_bytes(c.try_into().unwrap())).collect())
}

/// An id for a new segment, from the process, a counter and the clock —
/// unique within an index without needing to look at what is already there.
pub fn new_segment_id(counter: u64) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    format!("{:08x}{:04x}{:04x}", nanos & 0xffff_ffff, std::process::id() & 0xffff, counter & 0xffff)
}

/// An open segment: its manifest entry and its mapping.
pub struct Segment {
    pub meta: SegmentMeta,
    pub data: MmapPostingData,
    /// `meta.deleted` as a set, for the search filter.
    deleted: HashSet<u64>,
    /// The segment's record ids, sorted — read on the first deletion or
    /// update, never for a search.
    ids: std::cell::OnceCell<Vec<u64>>,
    base: PathBuf,
}

impl Segment {
    pub fn open(base: &Path, meta: SegmentMeta) -> Result<Self, String> {
        let data = MmapPostingData::open(&base.join(segment_file(&meta.id)))?;
        let deleted = meta.deleted.iter().copied().collect();
        Ok(Self { meta, data, deleted, ids: std::cell::OnceCell::new(), base: base.to_path_buf() })
    }

    /// Whether this segment still answers for `id`.
    pub fn is_live(&self, id: u64) -> bool {
        !self.deleted.contains(&id)
    }

    /// The segment's ids, loaded on first use.
    pub fn ids(&self) -> Result<&[u64], String> {
        if let Some(ids) = self.ids.get() {
            return Ok(ids);
        }
        let path = self.base.join(ids_file(&self.meta.id));
        let data = std::fs::read(&path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let ids = decode_ids(&data)?;
        Ok(self.ids.get_or_init(|| ids))
    }

    /// Whether this segment was written with `id` (deleted or not).
    pub fn holds(&self, id: u64) -> Result<bool, String> {
        Ok(self.ids()?.binary_search(&id).is_ok())
    }

    /// Mark `id` deleted here. Answers whether it changed anything.
    pub fn tombstone(&mut self, id: u64) -> bool {
        if !self.deleted.insert(id) {
            return false;
        }
        if let Err(pos) = self.meta.deleted.binary_search(&id) {
            self.meta.deleted.insert(pos, id);
        }
        true
    }

    pub fn path(&self) -> PathBuf {
        self.base.join(segment_file(&self.meta.id))
    }

    pub fn ids_path(&self) -> PathBuf {
        self.base.join(ids_file(&self.meta.id))
    }
}

// ---------------------------------------------------------------------------
// Merge
// ---------------------------------------------------------------------------

/// Merge segments into one, applying their tombstones.
///
/// This is the operation the format was shaped for: every segment's
/// dimension table is sorted by **global token id**, so the merge walks the
/// tables together and concatenates the posting lists of the same token —
/// nothing is remapped, no id is rewritten, and no dictionary is rebuilt.
/// Merging two *indexes* is the same call with their segments as input,
/// which is why segments, index fusion and delta sync are one mechanism and
/// not three.
///
/// Within a token, entries stay sorted by record id and the newest segment
/// wins a tie — a document updated in a later segment already had its older
/// copy tombstoned, so a tie only happens between a segment and itself.
pub fn merge_segments(
    base: &Path,
    inputs: &[&Segment],
    new_id: &str,
) -> Result<SegmentMeta, String> {
    use crate::wand::Postings;

    // Every token any input holds, once, in order — a k-way walk over
    // tables that are already sorted.
    let mut cursors: Vec<std::iter::Peekable<_>> =
        inputs.iter().map(|s| s.data.tokens().peekable()).collect();
    let mut tokens: Vec<u32> = Vec::new();
    loop {
        let next = cursors.iter_mut()
            .filter_map(|c| c.peek().map(|(t, _)| *t))
            .min();
        let Some(token) = next else { break };
        for c in cursors.iter_mut() {
            if c.peek().is_some_and(|(t, _)| *t == token) {
                c.next();
            }
        }
        tokens.push(token);
    }

    // The postings of each token, live entries only.
    let mut postings: Vec<Postings> = Vec::with_capacity(tokens.len());
    let mut ids: std::collections::BTreeSet<u64> = std::collections::BTreeSet::new();
    let mut pairs: Vec<(u64, f32)> = Vec::new();
    for &token in &tokens {
        pairs.clear();
        for seg in inputs {
            for e in seg.data.entries_of_token(token) {
                if seg.is_live(e.record_id) {
                    pairs.push((e.record_id, e.weight));
                    ids.insert(e.record_id);
                }
            }
        }
        // Inputs are each sorted by id, their union is not.
        pairs.sort_by_key(|(id, _)| *id);
        postings.push(Postings::from_sorted_pairs(&pairs));
    }

    let ids: Vec<u64> = ids.into_iter().collect();
    crate::mmap_index::write_mmap_file(
        &base.join(segment_file(new_id)),
        &postings,
        &tokens,
        ids.len() as u32,
    )?;
    write_file_atomic(&base.join(ids_file(new_id)), &encode_ids(&ids))?;

    Ok(SegmentMeta { id: new_id.to_string(), num_vectors: ids.len() as u32, deleted: Vec::new() })
}
