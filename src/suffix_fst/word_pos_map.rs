//! Word-position map: for each (doc_id, position) → word_id within doc.
//!
//! Enables O(1) verification that two adjacent tokens are from the same word
//! (intra-word) or different words (inter-word). Used by cross-token chain
//! verification to filter false positives from content-prefix ordinals.
//!
//! word_id is a per-doc counter incremented at each new segment (word boundary).
//! Two positions with the same word_id = same word = intra-word = always valid.
//!
//! Format: same as PosMap but stores word_id instead of ordinal.
//! ```text
//! [4 bytes] magic "WMAP"
//! [4 bytes] num_docs: u32 LE
//! [8 bytes × (num_docs + 1)] offset table
//! Data section (per doc):
//!   [4 bytes × num_tokens] word_ids: u32 LE, one per position
//! ```

use super::index_registry::{SfxIndexFile, MergeStrategy};

/// Builds a word-position map during indexation.
pub struct WordPosMapWriter {
    docs: Vec<Vec<u32>>,
}

impl Default for WordPosMapWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl WordPosMapWriter {
    pub fn new() -> Self {
        Self { docs: Vec::new() }
    }

    /// Record that position `pos` in `doc_id` belongs to word `word_id`.
    pub fn add(&mut self, doc_id: u32, position: u32, word_id: u32) {
        let d = doc_id as usize;
        if d >= self.docs.len() {
            self.docs.resize(d + 1, Vec::new());
        }
        let p = position as usize;
        let doc = &mut self.docs[d];
        if p >= doc.len() {
            doc.resize(p + 1, u32::MAX);
        }
        doc[p] = word_id;
    }

    /// Serialize to binary format.
    pub fn serialize(&self) -> Vec<u8> {
        let num_docs = self.docs.len() as u32;
        let header_size = 4 + 4 + (num_docs as usize + 1) * 8;
        let data_size: usize = self.docs.iter().map(|d| d.len() * 4).sum();
        let mut buf = Vec::with_capacity(header_size + data_size);

        buf.extend_from_slice(b"WMAP");
        buf.extend_from_slice(&num_docs.to_le_bytes());

        // Offset table
        let mut offset = 0u64;
        for doc in &self.docs {
            buf.extend_from_slice(&offset.to_le_bytes());
            offset += (doc.len() * 4) as u64;
        }
        buf.extend_from_slice(&offset.to_le_bytes()); // sentinel

        // Data
        for doc in &self.docs {
            for &wid in doc {
                buf.extend_from_slice(&wid.to_le_bytes());
            }
        }

        buf
    }
}

/// Reader for word-position map. O(1) lookup.
pub struct WordPosMapReader<'a> {
    num_docs: u32,
    offsets: &'a [u8],
    data: &'a [u8],
}

impl<'a> WordPosMapReader<'a> {
    pub fn open(bytes: &'a [u8]) -> Option<Self> {
        if bytes.len() < 8 { return None; }
        if &bytes[0..4] != b"WMAP" { return None; }
        let num_docs = u32::from_le_bytes(bytes[4..8].try_into().ok()?);
        let offsets_size = (num_docs as usize + 1) * 8;
        if bytes.len() < 8 + offsets_size { return None; }
        Some(Self {
            num_docs,
            offsets: &bytes[8..8 + offsets_size],
            data: &bytes[8 + offsets_size..],
        })
    }

    /// Get word_id at (doc_id, position). Returns None if out of range.
    /// Number of documents covered.
    pub fn num_docs(&self) -> u32 { self.num_docs }

    /// Number of recorded positions for a document — the merge walks these rather
    /// than probing position by position with no idea where to stop.
    pub fn num_positions(&self, doc_id: u32) -> u32 {
        if doc_id >= self.num_docs { return 0; }
        let i = doc_id as usize * 8;
        let start = u64::from_le_bytes(self.offsets[i..i + 8].try_into().unwrap()) as usize;
        let end = u64::from_le_bytes(self.offsets[i + 8..i + 16].try_into().unwrap()) as usize;
        ((end - start) / 4) as u32
    }

    pub fn word_at(&self, doc_id: u32, position: u32) -> Option<u32> {
        if doc_id >= self.num_docs { return None; }
        let start = self.read_offset(doc_id) as usize;
        let end = self.read_offset(doc_id + 1) as usize;
        let num_positions = (end - start) / 4;
        if position as usize >= num_positions { return None; }
        let pos = start + position as usize * 4;
        if pos + 4 > self.data.len() { return None; }
        let wid = u32::from_le_bytes(self.data[pos..pos + 4].try_into().ok()?);
        if wid == u32::MAX { None } else { Some(wid) }
    }

    /// Check if two adjacent positions are in the same word.
    pub fn same_word(&self, doc_id: u32, pos_a: u32, pos_b: u32) -> bool {
        match (self.word_at(doc_id, pos_a), self.word_at(doc_id, pos_b)) {
            (Some(a), Some(b)) => a == b,
            _ => true, // no data → don't filter
        }
    }

    fn read_offset(&self, idx: u32) -> u64 {
        let pos = idx as usize * 8;
        u64::from_le_bytes(self.offsets[pos..pos + 8].try_into().unwrap())
    }
}

/// SfxIndexFile implementation — EventDriven, built from postings.
pub struct WordPosMapIndex {
    writer: WordPosMapWriter,
}

impl WordPosMapIndex {
    pub fn new() -> Self {
        Self { writer: WordPosMapWriter::new() }
    }
}

impl SfxIndexFile for WordPosMapIndex {
    fn id(&self) -> &'static str { "word_pos_map" }
    fn extension(&self) -> &'static str { "word_pos_map" }
    fn merge_strategy(&self) -> MergeStrategy { MergeStrategy::OrMergeWithRemap }
    fn prebuilt_by_collector(&self) -> bool { true }
    fn serialize(&self) -> Vec<u8> { self.writer.serialize() }
}

// ─── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_roundtrip() {
        let mut w = WordPosMapWriter::new();
        // Doc 0: "mutex_lock" → word 0 = "mutex_" (2 chunks), word 1 = "lock" (1 chunk)
        w.add(0, 0, 0); // pos 0 → word 0
        w.add(0, 1, 1); // pos 1 → word 1
        // Doc 1: "hello_world" → word 0 = "hello_" (1 chunk), word 1 = "world" (1 chunk)
        w.add(1, 0, 0);
        w.add(1, 1, 1);

        let data = w.serialize();
        let r = WordPosMapReader::open(&data).unwrap();

        assert_eq!(r.word_at(0, 0), Some(0));
        assert_eq!(r.word_at(0, 1), Some(1));
        assert_eq!(r.word_at(1, 0), Some(0));
        assert_eq!(r.word_at(1, 1), Some(1));
        assert_eq!(r.word_at(0, 99), None);
        assert_eq!(r.word_at(99, 0), None);
    }

    #[test]
    fn test_same_word() {
        let mut w = WordPosMapWriter::new();
        // "internationalization" → 3 chunks, all word 0
        w.add(0, 0, 0);
        w.add(0, 1, 0);
        w.add(0, 2, 0);
        // "mutex_lock" → word 1 (1 chunk), word 2 (1 chunk)
        w.add(0, 3, 1);
        w.add(0, 4, 2);

        let data = w.serialize();
        let r = WordPosMapReader::open(&data).unwrap();

        // Intra-word
        assert!(r.same_word(0, 0, 1));  // chunks 0,1 of same word
        assert!(r.same_word(0, 1, 2));  // chunks 1,2 of same word
        // Inter-word
        assert!(!r.same_word(0, 2, 3)); // end of word 0 → start of word 1
        assert!(!r.same_word(0, 3, 4)); // word 1 → word 2
    }
}
