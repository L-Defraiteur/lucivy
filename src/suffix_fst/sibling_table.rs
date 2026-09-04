//! Sibling table: maps each token ordinal to its possible next-token successors.
//!
//! Built during SFX construction from consecutive tokens observed in the same value.
//! Used by cross-token search to follow token chains without query-time graph/DP.
//!
//! Format v1 (read-only now):
//! ```text
//! [4 bytes] num_ordinals
//! [4 bytes × (num_ordinals + 1)] offset table (byte offset into entries_data)
//! Entries data (per ordinal, variable length):
//!   Sequence of SiblingEntry:
//!     [4 bytes] next_ordinal
//!     [2 bytes] gap_len (0 = contiguous, >0 = separator bytes between tokens)
//! ```
//!
//! Format `SIB2` (written since 25 August 2026): the six fixed bytes become a
//! varint. This file is the second most-read sidecar of a query — 176 MB of
//! its 263 MB are faulted in by a common one
//! (`lucivy_core/tests/test_touched_bytes.rs`) — and it is the cheapest to
//! encode: an ordinal's entries are sorted and deduplicated, so
//! `next_ordinal` only grows, and every reader walks them start to end
//! (`siblings`, `contiguous_siblings`), so there is no random access to keep
//! and no checkpoint to pay for, unlike `.word_sfxpost` (WSP3).
//!
//! ```text
//! [4 bytes] 0xFFFFFFFF        (a v1 file's first word is num_ordinals, and
//!                              u32::MAX ordinals would need a 16 GB offset
//!                              table: unambiguous)
//! [4 bytes] magic "SIB2"
//! [4 bytes] num_ordinals
//! [4 bytes × (num_ordinals + 1)] offset table (byte offset into entries_data)
//! Entries data (per ordinal): sequence of
//!   [varint] (next_ordinal - previous) << 1 | (gap_len != 0)
//!   [varint] gap_len, only when the low bit above is set
//! ```
//!
//! `gap_len` spends two bytes on a value that is 0 for the overwhelming
//! majority of links (contiguous tokens) — `contiguous_siblings`, the hot
//! reader, keeps exactly those. One bit carries that case, and the delta on
//! `next_ordinal` shrinks the rest.
//!
//! Format `SIB3` (written since 4 September 2026, whenever no link carries a
//! gap): the same header with magic "SIB3", and one varint per entry —
//! `next_ordinal - previous`, nothing else. A v3 segment never has gaps to
//! store: its collector used the field to carry the destination's content
//! length, which the DFS now reads from `.termtexts` META
//! (`own_len - sep_len`), so the field was 31 % of the entries for a value
//! held elsewhere. The writer picks `SIB2` on its own when a gap is present
//! (the v2 pipeline), so both shapes stay readable and nothing chooses.

use super::varint::{read_varint, write_varint};

/// First word of a `SIB2` file. A v1 file starts with `num_ordinals`, and
/// `u32::MAX` ordinals would need a 16 GB offset table, so no v1 file can
/// begin with this.
const V2_SENTINEL: u32 = u32::MAX;
const MAGIC_V2: &[u8; 4] = b"SIB2";
const MAGIC_V3: &[u8; 4] = b"SIB3";
/// Bytes of one v1 entry: `next_ordinal` (u32) + `gap_len` (u16).
const V1_ENTRY_SIZE: usize = 6;

/// A single sibling link: this token is followed by `next_ordinal` with `gap_len` bytes between.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SiblingEntry {
    /// Ordinal of the next token in the original text.
    pub next_ordinal: u32,
    /// Number of bytes between the end of this token and the start of the next.
    /// 0 = contiguous (cross-token search viable).
    pub gap_len: u16,
}

/// Builder: collects sibling pairs during indexation, serializes to binary.
/// Uses a flat buffer (no HashMap/HashSet) — sort + dedup at serialize time.
pub struct SiblingTableWriter {
    /// Flat buffer: (ordinal, next_ordinal, gap_len). Unsorted, with potential dups.
    pairs: Vec<(u32, u32, u16)>,
    num_ordinals: u32,
}

impl SiblingTableWriter {
    /// Create a new writer for `num_ordinals` unique tokens.
    pub fn new(num_ordinals: u32) -> Self {
        Self {
            pairs: Vec::new(),
            num_ordinals,
        }
    }

    /// Record that `ordinal` is followed by `next_ordinal` with `gap_len` bytes between.
    pub fn add(&mut self, ordinal: u32, next_ordinal: u32, gap_len: u16) {
        self.pairs.push((ordinal, next_ordinal, gap_len));
    }

    /// Serialize to binary format. Sorts and deduplicates the flat buffer.
    pub fn serialize(&mut self) -> Vec<u8> {
        // Sort by (ordinal, next_ordinal, gap_len) then dedup
        self.pairs.sort_unstable();
        self.pairs.dedup();
        let with_gaps = self.pairs.iter().any(|&(_, _, g)| g != 0);

        let num = self.num_ordinals;
        let header_size = 12 + (num as usize + 1) * 4;
        let mut offsets: Vec<u32> = Vec::with_capacity(num as usize + 1);
        let mut entries_data: Vec<u8> = Vec::new();

        let mut cursor = 0usize;
        for ord in 0..num {
            offsets.push(entries_data.len() as u32);
            // `next_ordinal` restarts from zero at each ordinal: the deltas of
            // one ordinal never depend on the previous one's, so a reader can
            // start at any offset of the table.
            let mut prev_next = 0u32;
            while cursor < self.pairs.len() && self.pairs[cursor].0 == ord {
                let (_, next_ord, gap_len) = self.pairs[cursor];
                let delta = next_ord.wrapping_sub(prev_next) as u64;
                if with_gaps {
                    write_varint(&mut entries_data, (delta << 1) | u64::from(gap_len != 0));
                    if gap_len != 0 {
                        write_varint(&mut entries_data, gap_len as u64);
                    }
                } else {
                    write_varint(&mut entries_data, delta);
                }
                prev_next = next_ord;
                cursor += 1;
            }
        }
        // u32 offsets: refuse a table past 4 GB rather than write a wrapped one.
        assert!(
            entries_data.len() <= u32::MAX as usize,
            "sibling table: {} bytes exceed the 32-bit offset table",
            entries_data.len()
        );
        offsets.push(entries_data.len() as u32); // sentinel

        let mut buf = Vec::with_capacity(header_size + entries_data.len());
        buf.extend_from_slice(&V2_SENTINEL.to_le_bytes());
        buf.extend_from_slice(if with_gaps { MAGIC_V2 } else { MAGIC_V3 });
        buf.extend_from_slice(&num.to_le_bytes());
        for off in &offsets {
            buf.extend_from_slice(&off.to_le_bytes());
        }
        buf.extend_from_slice(&entries_data);
        buf
    }
}

/// Reader: O(1) lookup of sibling entries by ordinal.
pub struct SiblingTableReader<'a> {
    num_ordinals: u32,
    offsets: &'a [u8],      // (num_ordinals + 1) × 4 bytes
    entries_data: &'a [u8],
    /// `SIB2`/`SIB3`: varint entries. v1 files keep their fixed 6-byte records.
    v2: bool,
    /// `SIB3`: one delta per entry, no gap bit.
    no_gaps: bool,
}

impl<'a> SiblingTableReader<'a> {
    /// Open from raw bytes. Returns None if data is too small.
    pub fn open(data: &'a [u8]) -> Option<Self> {
        if data.len() < 4 {
            return None;
        }
        let v2 = u32::from_le_bytes(data[0..4].try_into().ok()?) == V2_SENTINEL;
        let mut no_gaps = false;
        let head = if v2 {
            if data.len() < 12 {
                return None;
            }
            match &data[4..8] {
                m if m == MAGIC_V2 => {}
                m if m == MAGIC_V3 => no_gaps = true,
                _ => return None,
            }
            12
        } else {
            4
        };
        let num_ordinals = u32::from_le_bytes(data[head - 4..head].try_into().ok()?);
        let offsets_size = (num_ordinals as usize + 1) * 4;
        if data.len() < head + offsets_size {
            return None;
        }
        let offsets = &data[head..head + offsets_size];
        let entries_data = &data[head + offsets_size..];
        Some(Self { num_ordinals, offsets, entries_data, v2, no_gaps })
    }

    /// Get all sibling entries for a given ordinal.
    pub fn siblings(&self, ordinal: u32) -> Vec<SiblingEntry> {
        if ordinal >= self.num_ordinals {
            return Vec::new();
        }
        let start = self.read_offset(ordinal) as usize;
        let end = self.read_offset(ordinal + 1) as usize;
        if start >= end || start >= self.entries_data.len() {
            return Vec::new();
        }
        let slice = &self.entries_data[start..end.min(self.entries_data.len())];
        let mut entries = Vec::new();
        let mut pos = 0;
        if self.no_gaps {
            let mut next_ordinal = 0u32;
            while pos < slice.len() {
                let Some(delta) = read_varint(slice, &mut pos) else { break };
                let Ok(delta) = u32::try_from(delta) else { break };
                next_ordinal = next_ordinal.wrapping_add(delta);
                entries.push(SiblingEntry { next_ordinal, gap_len: 0 });
            }
            return entries;
        }
        if self.v2 {
            let mut next_ordinal = 0u32;
            while pos < slice.len() {
                let Some(token) = read_varint(slice, &mut pos) else { break };
                // The delta is a u32 at the writer; wider means a corrupt
                // file, and a truncated delta would be a plausible wrong link.
                let Ok(delta) = u32::try_from(token >> 1) else { break };
                next_ordinal = next_ordinal.wrapping_add(delta);
                // `gap_len` is a u16 at the writer, so a wider value here means
                // a corrupt file: stop reading this ordinal rather than
                // truncate the value and hand back a plausible-looking link.
                let gap_len = if token & 1 == 1 {
                    match read_varint(slice, &mut pos).and_then(|g| u16::try_from(g).ok()) {
                        Some(g) => g,
                        None => break,
                    }
                } else {
                    0
                };
                entries.push(SiblingEntry { next_ordinal, gap_len });
            }
            return entries;
        }
        while pos + V1_ENTRY_SIZE <= slice.len() {
            let next_ordinal = u32::from_le_bytes(slice[pos..pos + 4].try_into().unwrap());
            let gap_len = u16::from_le_bytes(slice[pos + 4..pos + 6].try_into().unwrap());
            entries.push(SiblingEntry { next_ordinal, gap_len });
            pos += V1_ENTRY_SIZE;
        }
        entries
    }

    /// Get contiguous siblings only (gap_len == 0) — used by cross-token search.
    pub fn contiguous_siblings(&self, ordinal: u32) -> Vec<u32> {
        self.siblings(ordinal)
            .into_iter()
            .filter(|s| s.gap_len == 0)
            .map(|s| s.next_ordinal)
            .collect()
    }

    /// True for a `SIB3` file: links carry no gap.
    pub fn has_no_gaps(&self) -> bool {
        self.no_gaps
    }

    /// Number of ordinals in the table.
    pub fn num_ordinals(&self) -> u32 {
        self.num_ordinals
    }

    fn read_offset(&self, idx: u32) -> u32 {
        let pos = idx as usize * 4;
        u32::from_le_bytes(self.offsets[pos..pos + 4].try_into().unwrap())
    }
}

// ─────────────────────────────────────────────────────────────────────
// SfxIndexFile implementation
// ─────────────────────────────────────────────────────────────────────

/// Index file wrapper for sibling tables (SfxIndexFile trait).
pub struct SiblingIndex {
    data: Vec<u8>,
}

impl Default for SiblingIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl SiblingIndex {
    /// Creates a new sibling table index file instance.
    pub fn new() -> Self { Self { data: Vec::new() } }
}

impl super::index_registry::SfxIndexFile for SiblingIndex {
    fn id(&self) -> &'static str { "sibling" }
    fn extension(&self) -> &'static str { "sibling" }
    /// v2 only: the v3 pipeline writes `.sibling_v3` and no gap or separator map.
    fn written_for(&self, sfx_version: u8) -> bool { sfx_version < 3 }
    fn merge_strategy(&self) -> super::index_registry::MergeStrategy {
        super::index_registry::MergeStrategy::OrMergeWithRemap
    }
    fn prebuilt_by_collector(&self) -> bool { true }

    fn merge_from_sources(
        &mut self,
        sources: &[Option<&[u8]>],
        source_termtexts: &[Option<&[u8]>],
        token_to_new_ord: &dyn Fn(&str) -> Option<u32>,
    ) {
        use super::TermTextsReader;
        // Determine num_terms from the max new ordinal we'll see
        let mut max_ord = 0u32;

        for (seg_idx, src_opt) in sources.iter().enumerate() {
            let src = match src_opt { Some(s) => s, None => continue };
            let sib_table = match SiblingTableReader::open(src) { Some(t) => t, None => continue };
            let tt = match source_termtexts[seg_idx].and_then(TermTextsReader::open) {
                Some(t) => t, None => continue,
            };

            for old_ord in 0..sib_table.num_ordinals() {
                let text_a = match tt.text(old_ord) { Some(t) => t, None => continue };
                let new_a = match token_to_new_ord(text_a) { Some(o) => o, None => continue };
                if new_a > max_ord { max_ord = new_a; }

                for entry in sib_table.siblings(old_ord) {
                    let text_b = match tt.text(entry.next_ordinal) { Some(t) => t, None => continue };
                    let new_b = match token_to_new_ord(text_b) { Some(o) => o, None => continue };
                    if new_b > max_ord { max_ord = new_b; }
                }
            }
        }

        let mut writer = SiblingTableWriter::new(max_ord + 1);

        for (seg_idx, src_opt) in sources.iter().enumerate() {
            let src = match src_opt { Some(s) => s, None => continue };
            let sib_table = match SiblingTableReader::open(src) { Some(t) => t, None => continue };
            let tt = match source_termtexts[seg_idx].and_then(TermTextsReader::open) {
                Some(t) => t, None => continue,
            };

            for old_ord in 0..sib_table.num_ordinals() {
                let text_a = match tt.text(old_ord) { Some(t) => t, None => continue };
                let new_a = match token_to_new_ord(text_a) { Some(o) => o, None => continue };

                for entry in sib_table.siblings(old_ord) {
                    let text_b = match tt.text(entry.next_ordinal) { Some(t) => t, None => continue };
                    let new_b = match token_to_new_ord(text_b) { Some(o) => o, None => continue };
                    writer.add(new_a, new_b, entry.gap_len);
                }
            }
        }

        self.data = writer.serialize();
    }

    fn serialize(&self) -> Vec<u8> { self.data.clone() }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A gap-less table takes the `SIB3` layout and reads back the same
    /// links; one gap anywhere keeps `SIB2`, gaps included.
    #[test]
    fn gapless_tables_take_sib3_and_read_alike() {
        let links = [(0u32, 5u32), (0, 9), (0, 5), (2, 3), (2, 1_000_000), (4, 4)];
        let mut w3 = SiblingTableWriter::new(5);
        for &(a, b) in &links { w3.add(a, b, 0); }
        let d3 = w3.serialize();
        assert_eq!(&d3[4..8], b"SIB3");
        let mut w2 = SiblingTableWriter::new(5);
        for &(a, b) in &links { w2.add(a, b, 0); }
        w2.add(4, 2, 7);
        let d2 = w2.serialize();
        assert_eq!(&d2[4..8], b"SIB2");

        let r3 = SiblingTableReader::open(&d3).unwrap();
        let r2 = SiblingTableReader::open(&d2).unwrap();
        assert!(r3.has_no_gaps() && !r2.has_no_gaps());
        for ord in 0..5 {
            let a: Vec<u32> = r3.siblings(ord).iter().map(|s| s.next_ordinal).collect();
            let b: Vec<u32> = r2.siblings(ord).iter().filter(|s| s.gap_len == 0).map(|s| s.next_ordinal).collect();
            assert_eq!(a, b, "ordinal {ord}");
        }
        assert_eq!(r3.siblings(0).iter().map(|s| s.next_ordinal).collect::<Vec<_>>(), vec![5, 9]);
        assert_eq!(r3.siblings(2).iter().map(|s| s.next_ordinal).collect::<Vec<_>>(), vec![3, 1_000_000]);
        assert_eq!(r3.siblings(1), vec![]);
        assert_eq!(r2.siblings(4), vec![SiblingEntry { next_ordinal: 2, gap_len: 7 }, SiblingEntry { next_ordinal: 4, gap_len: 0 }]);
        assert!(r3.siblings(0).iter().all(|s| s.gap_len == 0));
        // 3 links × 1-2 bytes for ordinal 0 and 2 against the SIB2 shape with its gap bits.
        assert!(d3.len() < d2.len());
    }

    #[test]
    fn test_roundtrip_empty() {
        let mut writer = SiblingTableWriter::new(3);
        let data = writer.serialize();
        let reader = SiblingTableReader::open(&data).unwrap();
        assert_eq!(reader.num_ordinals(), 3);
        assert!(reader.siblings(0).is_empty());
        assert!(reader.siblings(1).is_empty());
        assert!(reader.siblings(2).is_empty());
    }

    #[test]
    fn test_roundtrip_single_sibling() {
        let mut writer = SiblingTableWriter::new(3);
        writer.add(0, 1, 0); // token 0 → token 1, contiguous
        let data = writer.serialize();
        let reader = SiblingTableReader::open(&data).unwrap();

        let s = reader.siblings(0);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0], SiblingEntry { next_ordinal: 1, gap_len: 0 });
        assert!(reader.siblings(1).is_empty());
        assert!(reader.siblings(2).is_empty());

        assert_eq!(reader.contiguous_siblings(0), vec![1]);
    }

    #[test]
    fn test_roundtrip_multiple_siblings() {
        let mut writer = SiblingTableWriter::new(4);
        // "get" → "Element" (contiguous) AND "Value" (contiguous)
        writer.add(0, 1, 0);
        writer.add(0, 2, 0);
        // "get" → "Config" (separated by space)
        writer.add(0, 3, 1);

        let data = writer.serialize();
        let reader = SiblingTableReader::open(&data).unwrap();

        let s = reader.siblings(0);
        assert_eq!(s.len(), 3);

        let contiguous = reader.contiguous_siblings(0);
        assert_eq!(contiguous.len(), 2);
        assert!(contiguous.contains(&1));
        assert!(contiguous.contains(&2));
    }

    /// The v1 writer, kept in the tests only: SIB2 has to keep reading the
    /// segments written before 25 August 2026.
    fn write_v1(num: u32, pairs: &mut Vec<(u32, u32, u16)>) -> Vec<u8> {
        pairs.sort_unstable();
        pairs.dedup();
        let mut offsets = Vec::new();
        let mut entries_data: Vec<u8> = Vec::new();
        let mut cursor = 0usize;
        for ord in 0..num {
            offsets.push(entries_data.len() as u32);
            while cursor < pairs.len() && pairs[cursor].0 == ord {
                entries_data.extend_from_slice(&pairs[cursor].1.to_le_bytes());
                entries_data.extend_from_slice(&pairs[cursor].2.to_le_bytes());
                cursor += 1;
            }
        }
        offsets.push(entries_data.len() as u32);
        let mut buf = Vec::new();
        buf.extend_from_slice(&num.to_le_bytes());
        for off in &offsets {
            buf.extend_from_slice(&off.to_le_bytes());
        }
        buf.extend_from_slice(&entries_data);
        buf
    }

    /// Links over several ordinals: contiguous ones (the common case), a few
    /// with a gap, ordinals with none, and one very long run.
    fn sample_pairs() -> Vec<(u32, u32, u16)> {
        let mut v = Vec::new();
        for ord in [0u32, 1, 5, 9] {
            for i in 0..(3 + ord * 7) {
                let next = ord * 13 + i * 3 + 1;
                let gap = if i % 5 == 4 { (i % 7 + 1) as u16 } else { 0 };
                v.push((ord, next, gap));
            }
        }
        // A run long enough that the deltas dominate, and a large ordinal.
        for i in 0..500u32 {
            v.push((3, i * 2 + 1, if i % 11 == 10 { 300 } else { 0 }));
        }
        v.push((7, 4_000_000, 65535));
        v.push((7, 4_000_001, 0));
        v
    }

    #[test]
    fn v2_matches_v1_exactly_and_is_smaller() {
        let num = 12;
        let pairs = sample_pairs();
        let mut w = SiblingTableWriter::new(num);
        for (o, n, g) in &pairs {
            w.add(*o, *n, *g);
        }
        let v2 = w.serialize();
        assert_eq!(&v2[0..4], &u32::MAX.to_le_bytes(), "SIB2 must start with the sentinel");
        assert_eq!(&v2[4..8], MAGIC_V2);

        let mut p = pairs.clone();
        let v1 = write_v1(num, &mut p);

        let r2 = SiblingTableReader::open(&v2).unwrap();
        let r1 = SiblingTableReader::open(&v1).unwrap();
        assert_eq!(r2.num_ordinals(), num);
        for ord in 0..num + 3 {
            assert_eq!(r2.siblings(ord), r1.siblings(ord), "ordinal {ord}: siblings differ");
            assert_eq!(
                r2.contiguous_siblings(ord), r1.contiguous_siblings(ord),
                "ordinal {ord}: contiguous_siblings differ"
            );
        }
        // The links of ordinal 3 are a long ascending run: exactly what the
        // deltas are for.
        assert_eq!(r2.siblings(3).len(), 500);
        assert!(v2.len() < v1.len(), "SIB2 {} B is not smaller than v1 {} B", v2.len(), v1.len());
    }

    #[test]
    fn v2_reader_still_reads_v1() {
        let mut pairs = sample_pairs();
        let v1 = write_v1(12, &mut pairs);
        let r = SiblingTableReader::open(&v1).unwrap();
        assert_eq!(r.siblings(0).len(), 3);
        assert_eq!(r.siblings(3).len(), 500);
        assert_eq!(r.siblings(7), vec![
            SiblingEntry { next_ordinal: 4_000_000, gap_len: 65535 },
            SiblingEntry { next_ordinal: 4_000_001, gap_len: 0 },
        ]);
    }

    #[test]
    fn v2_survives_a_truncated_file() {
        let mut w = SiblingTableWriter::new(12);
        for (o, n, g) in sample_pairs() {
            w.add(o, n, g);
        }
        let data = w.serialize();
        for cut in [data.len() / 3, data.len() / 2, data.len() - 1] {
            if let Some(r) = SiblingTableReader::open(&data[..cut]) {
                for ord in 0..12 {
                    let _ = r.siblings(ord);
                    let _ = r.contiguous_siblings(ord);
                }
            }
        }
    }

    #[test]
    fn test_out_of_bounds() {
        let mut writer = SiblingTableWriter::new(2);
        let data = writer.serialize();
        let reader = SiblingTableReader::open(&data).unwrap();
        assert!(reader.siblings(99).is_empty());
    }
}
