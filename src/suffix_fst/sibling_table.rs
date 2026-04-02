//! Sibling table: maps each token ordinal to its possible next-token successors.
//!
//! Built during SFX construction from consecutive tokens observed in the same value.
//! Used by cross-token search to follow token chains without query-time graph/DP.
//!
//! Format:
//! ```text
//! [4 bytes] num_ordinals
//! [4 bytes × (num_ordinals + 1)] offset table (byte offset into entries_data)
//! Entries data (per ordinal, variable length):
//!   Sequence of SiblingEntry:
//!     [4 bytes] next_ordinal
//!     [2 bytes] gap_len (0 = contiguous, >0 = separator bytes between tokens)
//! ```

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

        let num = self.num_ordinals;
        let header_size = 4 + (num as usize + 1) * 4;
        let mut offsets: Vec<u32> = Vec::with_capacity(num as usize + 1);
        let mut entries_data: Vec<u8> = Vec::new();

        let mut cursor = 0usize;
        for ord in 0..num {
            offsets.push(entries_data.len() as u32);
            while cursor < self.pairs.len() && self.pairs[cursor].0 == ord {
                let (_, next_ord, gap_len) = self.pairs[cursor];
                entries_data.extend_from_slice(&next_ord.to_le_bytes());
                entries_data.extend_from_slice(&gap_len.to_le_bytes());
                cursor += 1;
            }
        }
        offsets.push(entries_data.len() as u32); // sentinel

        let mut buf = Vec::with_capacity(header_size + entries_data.len());
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
}

impl<'a> SiblingTableReader<'a> {
    /// Open from raw bytes. Returns None if data is too small.
    pub fn open(data: &'a [u8]) -> Option<Self> {
        if data.len() < 4 {
            return None;
        }
        let num_ordinals = u32::from_le_bytes(data[0..4].try_into().ok()?);
        let offsets_size = (num_ordinals as usize + 1) * 4;
        if data.len() < 4 + offsets_size {
            return None;
        }
        let offsets = &data[4..4 + offsets_size];
        let entries_data = &data[4 + offsets_size..];
        Some(Self { num_ordinals, offsets, entries_data })
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
        while pos + 6 <= slice.len() {
            let next_ordinal = u32::from_le_bytes(slice[pos..pos + 4].try_into().unwrap());
            let gap_len = u16::from_le_bytes(slice[pos + 4..pos + 6].try_into().unwrap());
            entries.push(SiblingEntry { next_ordinal, gap_len });
            pos += 6;
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

    /// Number of ordinals in the table.
    pub fn num_ordinals(&self) -> u32 {
        self.num_ordinals
    }

    fn read_offset(&self, idx: u32) -> u32 {
        let pos = idx as usize * 4;
        u32::from_le_bytes(self.offsets[pos..pos + 4].try_into().unwrap())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn test_out_of_bounds() {
        let mut writer = SiblingTableWriter::new(2);
        let data = writer.serialize();
        let reader = SiblingTableReader::open(&data).unwrap();
        assert!(reader.siblings(99).is_empty());
    }
}

// ─────────────────────────────────────────────────────────────────────
// SfxIndexFile implementation
// ─────────────────────────────────────────────────────────────────────

pub struct SiblingIndex {
    data: Vec<u8>,
}

impl SiblingIndex {
    pub fn new() -> Self { Self { data: Vec::new() } }
}

impl super::index_registry::SfxIndexFile for SiblingIndex {
    fn id(&self) -> &'static str { "sibling" }
    fn extension(&self) -> &'static str { "sibling" }
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
            let tt = match source_termtexts[seg_idx].and_then(|b| TermTextsReader::open(b)) {
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
            let tt = match source_termtexts[seg_idx].and_then(|b| TermTextsReader::open(b)) {
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
