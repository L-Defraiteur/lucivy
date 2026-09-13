//! Derived group index of a version-8 `.sfx` parents table:
//! `dict-<g>.<field>.pidx` next to a shard dictionary's generation.
//!
//! A grouped record (`encode_parent_record_v8`) lists its overlap groups in
//! overlap order, each skipped by its byte length: reaching the group a
//! lookup wants reads every header before it. On the kernel's largest
//! generation, 10 306 records hold 256 groups or more (40 M of the 73 M
//! parents), and a dictionary lookup of a frequent short text walked
//! hundreds of headers — 52.7 s of CPU over the kernel at 16 threads
//! (`docs/13-09-2026/07` §5 quater). This file keeps, for every record of
//! more than `STRIDE` groups, one checkpoint per `STRIDE` groups: the
//! group's overlap, the byte position of its header and the previous
//! group's first ordinal (the header holds a delta). A lookup binary
//! searches the record, then the checkpoints, then reads at most `STRIDE`
//! headers — and stops at the first overlap past the one it wants.
//!
//! Derived: built from the table alone (`for_each_table_record`, no FST
//! walk), written by the compactions and folds as they write the table,
//! rebuilt in RAM by a reader that finds no file (an index written before
//! it works as is and gains the file at its next compaction), ignored by a
//! reader that predates it. The container format (8) does not change.
//!
//! Layout, little-endian:
//!
//! ```text
//! "PIDX" | version u8 = 1 | stride u8 | reserved u16
//! n_records u32
//! records[n]: table_offset u64, entry_start u32              (12 bytes)
//! n_entries u32
//! entries[m]: prev_first u32, header_pos u32, ov_len u8, ov[4]   (13 bytes)
//! ```
//!
//! A record's entries run from its `entry_start` to the next record's (or
//! `n_entries`), entry `k` standing for group `k × stride`. Records are in
//! table order (offsets ascending), entries of a record in overlap order.

use common::OwnedBytes;

use super::builder_v3::{
    decode_group_v8_from, for_each_group_header_v8, for_each_table_record, read_record_head_v8,
    ParentEntryV3, MAX_OVERLAP_BYTES,
};

/// File magic.
pub const MAGIC: [u8; 4] = *b"PIDX";
/// Format version written.
pub const VERSION: u8 = 1;
/// Groups between two checkpoints; a record with at most this many groups
/// is not indexed (its scan is already this short).
pub const STRIDE: usize = 16;
/// The file extension (`dictionary_file_name(g, field, GROUP_INDEX_EXT)`).
pub const GROUP_INDEX_EXT: &str = "pidx";

const HEADER_LEN: usize = 12;
const RECORD_LEN: usize = 12;
const ENTRY_LEN: usize = 9 + MAX_OVERLAP_BYTES;

/// Builds the index record by record, as a compaction writes its table.
#[derive(Default)]
pub struct GroupIndexBuilder {
    records: Vec<u8>,
    entries: Vec<u8>,
    n_records: u32,
    n_entries: u32,
}

impl GroupIndexBuilder {
    /// An empty builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Consider the record the FST holds `table_offset` for (its bytes,
    /// length prefix excluded). Records must come in offset order.
    pub fn add(&mut self, table_offset: u64, record: &[u8]) {
        if record.is_empty() || record[0] & 0x80 == 0 {
            return;
        }
        let (_, count, _) = read_record_head_v8(record);
        if count <= STRIDE {
            return;
        }
        let entries_before = self.entries.len();
        let n_before = self.n_entries;
        let mut fits = true;
        for_each_group_header_v8(record, |g, header_pos, prev_first, ov| {
            if g % STRIDE != 0 {
                return;
            }
            if prev_first > u32::MAX as u64 || header_pos > u32::MAX as usize {
                fits = false;
                return;
            }
            self.entries.extend_from_slice(&(prev_first as u32).to_le_bytes());
            self.entries.extend_from_slice(&(header_pos as u32).to_le_bytes());
            self.entries.push(ov.len() as u8);
            let mut padded = [0u8; MAX_OVERLAP_BYTES];
            padded[..ov.len()].copy_from_slice(ov);
            self.entries.extend_from_slice(&padded);
            self.n_entries += 1;
        });
        if !fits {
            self.entries.truncate(entries_before);
            self.n_entries = n_before;
            return;
        }
        self.records.extend_from_slice(&table_offset.to_le_bytes());
        self.records.extend_from_slice(&n_before.to_le_bytes());
        self.n_records += 1;
    }

    /// The file's bytes.
    pub fn finish(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_LEN + self.records.len() + 4 + self.entries.len());
        out.extend_from_slice(&MAGIC);
        out.push(VERSION);
        out.push(STRIDE as u8);
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&self.n_records.to_le_bytes());
        out.extend_from_slice(&self.records);
        out.extend_from_slice(&self.n_entries.to_le_bytes());
        out.extend_from_slice(&self.entries);
        out
    }

    /// The index of a whole parents table (`lucivy_fst::OutputTable`
    /// layout) — what a reader builds when the file is missing.
    pub fn build_from_table(table: &[u8]) -> Vec<u8> {
        let mut b = Self::new();
        for_each_table_record(table, |offset, record| b.add(offset, record));
        b.finish()
    }
}

/// A group index, read in place.
#[derive(Clone)]
pub struct GroupIndex {
    bytes: OwnedBytes,
    n_records: usize,
    records_at: usize,
    n_entries: usize,
    entries_at: usize,
    stride: usize,
}

impl GroupIndex {
    /// Open the file's bytes; `None` when they are not a group index this
    /// reader understands (a reader then rebuilds in RAM).
    pub fn open(bytes: OwnedBytes) -> Option<Self> {
        let b = bytes.as_slice();
        if b.len() < HEADER_LEN || b[..4] != MAGIC || b[4] != VERSION {
            return None;
        }
        let stride = b[5] as usize;
        if stride == 0 {
            return None;
        }
        let n_records = u32::from_le_bytes(b[8..12].try_into().ok()?) as usize;
        let records_at = HEADER_LEN;
        let entries_count_at = records_at + n_records * RECORD_LEN;
        if b.len() < entries_count_at + 4 {
            return None;
        }
        let n_entries = u32::from_le_bytes(b[entries_count_at..entries_count_at + 4].try_into().ok()?) as usize;
        let entries_at = entries_count_at + 4;
        if b.len() < entries_at + n_entries * ENTRY_LEN {
            return None;
        }
        Some(Self { bytes, n_records, records_at, n_entries, entries_at, stride })
    }

    /// The index of `table`, built in RAM.
    pub fn build(table: &[u8]) -> Self {
        Self::open(OwnedBytes::new(GroupIndexBuilder::build_from_table(table)))
            .expect("a group index this builder wrote opens")
    }

    /// Records indexed.
    pub fn num_records(&self) -> usize {
        self.n_records
    }

    /// Checkpoints held.
    pub fn num_entries(&self) -> usize {
        self.n_entries
    }

    /// Bytes of the index.
    pub fn num_bytes(&self) -> usize {
        self.bytes.len()
    }

    #[inline]
    fn record_offset(&self, i: usize) -> u64 {
        let at = self.records_at + i * RECORD_LEN;
        u64::from_le_bytes(self.bytes.as_slice()[at..at + 8].try_into().unwrap())
    }

    #[inline]
    fn record_entry_start(&self, i: usize) -> usize {
        let at = self.records_at + i * RECORD_LEN + 8;
        u32::from_le_bytes(self.bytes.as_slice()[at..at + 4].try_into().unwrap()) as usize
    }

    #[inline]
    fn entry(&self, k: usize) -> (u64, usize, &[u8]) {
        let at = self.entries_at + k * ENTRY_LEN;
        let b = &self.bytes.as_slice()[at..at + ENTRY_LEN];
        let prev_first = u32::from_le_bytes(b[0..4].try_into().unwrap()) as u64;
        let header_pos = u32::from_le_bytes(b[4..8].try_into().unwrap()) as usize;
        let ov_len = (b[8] as usize).min(MAX_OVERLAP_BYTES);
        (prev_first, header_pos, &b[9..9 + ov_len])
    }

    /// The checkpoints of the record at `table_offset`, `None` when it is
    /// not indexed.
    fn entries_of(&self, table_offset: u64) -> Option<(usize, usize)> {
        let (mut lo, mut hi) = (0usize, self.n_records);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            match self.record_offset(mid).cmp(&table_offset) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => {
                    let start = self.record_entry_start(mid);
                    let end = if mid + 1 < self.n_records { self.record_entry_start(mid + 1) } else { self.n_entries };
                    return Some((start, end));
                }
            }
        }
        None
    }

    /// The parents of `record` (the bytes the FST holds `table_offset`
    /// for) whose overlap is exactly `want`; `None` when the record is not
    /// indexed — the caller then scans it (`decode_parent_entries_v8_overlap`).
    pub fn parents_with_overlap(&self, table_offset: u64, record: &[u8], key: &[u8], want: &[u8]) -> Option<Vec<ParentEntryV3>> {
        let (start, end) = self.entries_of(table_offset)?;
        // Last checkpoint whose overlap is not past `want`.
        let (mut lo, mut hi) = (start, end);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.entry(mid).2 <= want { lo = mid + 1 } else { hi = mid }
        }
        if lo == start {
            return Some(Vec::new());
        }
        let k = lo - 1;
        let (prev_first, header_pos, _) = self.entry(k);
        let (_, count, _) = read_record_head_v8(record);
        Some(decode_group_v8_from(record, header_pos, prev_first, (k - start) * self.stride, count, want, key))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suffix_fst::builder_v3::{decode_parent_entries_v8_overlap, decode_parent_entries_v8_where, encode_parent_record_v8};
    use lucivy_fst::OutputTableBuilder;

    fn parent(ordinal: u64, sti: u16, ov: &[u8]) -> ParentEntryV3 {
        let mut overlap = [0u8; MAX_OVERLAP_BYTES];
        overlap[..ov.len()].copy_from_slice(ov);
        ParentEntryV3 { raw_ordinal: ordinal, sti, own_len: 5, sep_len: 1, overlap_len: ov.len() as u8, overlap, is_word_start: sti == 0 }
    }

    /// A record of `groups` overlaps, a few parents each, ordinals spread.
    fn record(groups: usize, key: &[u8], seed: u64) -> (Vec<u8>, Vec<Vec<u8>>) {
        let mut parents = Vec::new();
        let mut overlaps = Vec::new();
        for g in 0..groups {
            let ov = vec![b'a' + (g % 26) as u8, b'a' + ((g / 26) % 26) as u8, if g % 3 == 0 { b'_' } else { b'x' }];
            let ov = &ov[..1 + (g % 3)];
            overlaps.push(ov.to_vec());
            for i in 0..1 + (g * 7 + seed as usize) % 5 {
                parents.push(parent(seed * 100_000 + (g * 31 + i * 17) as u64, ((i * 3) % 4) as u16, ov));
            }
        }
        (encode_parent_record_v8(&mut parents, key), overlaps)
    }

    #[test]
    fn index_answers_like_the_scan() {
        let key = b"\x01hello";
        let mut table = OutputTableBuilder::new();
        let mut recs = Vec::new();
        for (i, &groups) in [1usize, 5, 16, 17, 33, 100, 300].iter().enumerate() {
            let (rec, ovs) = record(groups, key, i as u64 + 1);
            let off = table.add(&rec);
            recs.push((off, rec, ovs, groups));
        }
        let table = table.into_inner();
        let index = GroupIndex::build(&table);
        assert_eq!(index.num_records(), 4, "records over {STRIDE} groups");
        for (off, rec, ovs, groups) in &recs {
            let mut probes: Vec<Vec<u8>> = ovs.clone();
            probes.push(b"".to_vec());
            probes.push(b"zzzz".to_vec());
            probes.push(b"a".to_vec());
            probes.push(b"ab".to_vec());
            probes.push(b"m_".to_vec());
            for want in &probes {
                let expected = decode_parent_entries_v8_where(rec, key, |ov| ov == want.as_slice());
                let scanned = decode_parent_entries_v8_overlap(rec, key, want);
                assert_eq!(scanned, expected, "scan, {groups} groups, want {want:?}");
                match index.parents_with_overlap(*off, rec, key, want) {
                    Some(found) => {
                        assert!(*groups > STRIDE);
                        assert_eq!(found, expected, "index, {groups} groups, want {want:?}");
                    }
                    None => assert!(*groups <= STRIDE, "{groups} groups not indexed"),
                }
            }
        }
    }

    #[test]
    fn incremental_build_equals_table_build() {
        let key = b"\x00ab";
        let mut table = OutputTableBuilder::new();
        let mut b = GroupIndexBuilder::new();
        for i in 0..40usize {
            let (rec, _) = record(3 + i * 2, key, i as u64);
            let off = table.add(&rec);
            b.add(off, &rec);
        }
        let table = table.into_inner();
        assert_eq!(b.finish(), GroupIndexBuilder::build_from_table(&table));
        let index = GroupIndex::build(&table);
        assert_eq!(index.num_records(), (0..40usize).filter(|i| 3 + i * 2 > STRIDE).count());
        assert!(index.num_entries() > index.num_records());
    }

    #[test]
    fn foreign_bytes_do_not_open() {
        assert!(GroupIndex::open(OwnedBytes::new(b"SFX3....".to_vec())).is_none());
        assert!(GroupIndex::open(OwnedBytes::new(Vec::new())).is_none());
        let empty = GroupIndexBuilder::new().finish();
        let idx = GroupIndex::open(OwnedBytes::new(empty)).unwrap();
        assert_eq!((idx.num_records(), idx.num_entries()), (0, 0));
        assert!(idx.parents_with_overlap(0, &[1, 0, 0, 0, 0], b"", b"").is_none());
    }
}
