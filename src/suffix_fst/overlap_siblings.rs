//! Overlap sibling table: maps each content ordinal to all extended ordinals
//! that share the same content (text[..own_len]) but have different overlaps.
//!
//! With content ordinals, all overlap variants share one content ordinal and
//! one aggregated posting list. The sibling table provides the reverse mapping:
//! given a content ordinal, find all extended ordinals (= all FST keys) that
//! share it. This is useful for query-time operations that need to distinguish
//! between overlap variants (e.g., validating that specific overlap bytes match).
//!
//! Format:
//! ```text
//! [4 bytes] num_content_ords
//! [4 bytes × (num_content_ords + 1)] offset table (byte offset into entries)
//! Entries (per content ordinal, variable length):
//!   [4 bytes × N] extended intern ordinals in this group
//! ```
//!
//! Built during `into_data()` from the content_key_map grouping.

/// Builder: collects (content_ord, intern_ord) pairs, serializes to binary.
pub struct OverlapSiblingWriter {
    /// For each content ordinal: list of intern ordinals sharing it.
    groups: Vec<Vec<u32>>,
}

impl OverlapSiblingWriter {
    pub fn new(num_content_ords: usize) -> Self {
        Self {
            groups: vec![Vec::new(); num_content_ords],
        }
    }

    /// Record that `intern_ord` belongs to content group `content_ord`.
    pub fn add(&mut self, content_ord: u32, intern_ord: u32) {
        if (content_ord as usize) < self.groups.len() {
            self.groups[content_ord as usize].push(intern_ord);
        }
    }

    /// Number of content ordinals with more than one overlap variant.
    pub fn num_multi_groups(&self) -> usize {
        self.groups.iter().filter(|g| g.len() > 1).count()
    }

    /// Serialize to binary format.
    pub fn serialize(&self) -> Vec<u8> {
        let num = self.groups.len() as u32;
        let mut offsets: Vec<u32> = Vec::with_capacity(self.groups.len() + 1);
        let mut entries: Vec<u8> = Vec::new();

        for group in &self.groups {
            offsets.push(entries.len() as u32);
            for &intern_ord in group {
                entries.extend_from_slice(&intern_ord.to_le_bytes());
            }
        }
        offsets.push(entries.len() as u32); // sentinel

        let header_size = 4 + (self.groups.len() + 1) * 4;
        let mut buf = Vec::with_capacity(header_size + entries.len());
        buf.extend_from_slice(&num.to_le_bytes());
        for off in &offsets {
            buf.extend_from_slice(&off.to_le_bytes());
        }
        buf.extend_from_slice(&entries);
        buf
    }
}

/// Reader: O(1) lookup of overlap siblings by content ordinal.
pub struct OverlapSiblingReader<'a> {
    num_content_ords: u32,
    offsets: &'a [u8],
    entries: &'a [u8],
}

impl<'a> OverlapSiblingReader<'a> {
    /// Open from raw bytes. Returns None if data is too small.
    pub fn open(data: &'a [u8]) -> Option<Self> {
        if data.len() < 4 { return None; }
        let num = u32::from_le_bytes(data[0..4].try_into().ok()?);
        let offsets_size = (num as usize + 1) * 4;
        if data.len() < 4 + offsets_size { return None; }
        Some(Self {
            num_content_ords: num,
            offsets: &data[4..4 + offsets_size],
            entries: &data[4 + offsets_size..],
        })
    }

    /// Get all intern ordinals that share the given content ordinal.
    /// Returns empty if content_ord is out of range or has no siblings.
    pub fn siblings(&self, content_ord: u32) -> Vec<u32> {
        if content_ord >= self.num_content_ords { return Vec::new(); }
        let start = self.read_offset(content_ord) as usize;
        let end = self.read_offset(content_ord + 1) as usize;
        if start >= end || start >= self.entries.len() { return Vec::new(); }
        let slice = &self.entries[start..end.min(self.entries.len())];
        slice.chunks_exact(4)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
            .collect()
    }

    /// True if this content ordinal has multiple overlap variants.
    pub fn has_siblings(&self, content_ord: u32) -> bool {
        if content_ord >= self.num_content_ords { return false; }
        let start = self.read_offset(content_ord) as usize;
        let end = self.read_offset(content_ord + 1) as usize;
        (end - start) > 4 // more than one u32 entry
    }

    /// Number of content ordinals in the table.
    pub fn num_content_ords(&self) -> u32 {
        self.num_content_ords
    }

    fn read_offset(&self, idx: u32) -> u32 {
        let pos = idx as usize * 4;
        u32::from_le_bytes(self.offsets[pos..pos + 4].try_into().unwrap())
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_roundtrip_empty() {
        let writer = OverlapSiblingWriter::new(3);
        let data = writer.serialize();
        let reader = OverlapSiblingReader::open(&data).unwrap();
        assert_eq!(reader.num_content_ords(), 3);
        assert!(reader.siblings(0).is_empty());
        assert!(!reader.has_siblings(0));
    }

    #[test]
    fn test_single_variant() {
        let mut writer = OverlapSiblingWriter::new(2);
        writer.add(0, 5); // content_ord 0 → intern_ord 5
        writer.add(1, 8); // content_ord 1 → intern_ord 8
        let data = writer.serialize();
        let reader = OverlapSiblingReader::open(&data).unwrap();

        assert_eq!(reader.siblings(0), vec![5]);
        assert!(!reader.has_siblings(0)); // only one variant
        assert_eq!(reader.siblings(1), vec![8]);
    }

    #[test]
    fn test_multiple_variants() {
        let mut writer = OverlapSiblingWriter::new(2);
        // content_ord 0 has 3 overlap variants
        writer.add(0, 5);  // "ptr<Co"
        writer.add(0, 12); // "ptr<ra"
        writer.add(0, 17); // "ptr<Va"
        // content_ord 1 has 1 variant
        writer.add(1, 8);
        let data = writer.serialize();
        let reader = OverlapSiblingReader::open(&data).unwrap();

        assert_eq!(reader.siblings(0), vec![5, 12, 17]);
        assert!(reader.has_siblings(0));
        assert_eq!(reader.siblings(1), vec![8]);
        assert!(!reader.has_siblings(1));
        assert_eq!(writer.num_multi_groups(), 1);
    }

    #[test]
    fn test_out_of_bounds() {
        let writer = OverlapSiblingWriter::new(2);
        let data = writer.serialize();
        let reader = OverlapSiblingReader::open(&data).unwrap();
        assert!(reader.siblings(99).is_empty());
        assert!(!reader.has_siblings(99));
    }
}
