//! Word Map: structural verification for cross-token chain matches.
//!
//! Two tables:
//! - **ChunkWordMap**: content_ordinal → [(word_id, chunk_index, total_chunks)]
//!   Which words each ordinal appears in, and at which chunk position.
//! - **NextWordMap**: word_id → [next_word_ids]
//!   Which words can follow a given word in the indexed corpus.
//!
//! A "word" = a segment from the tokenizer (content + trailing sep).
//! word_id is assigned by interning the segment text (lowered).

// ─── ChunkWordMap ──────────────────────────────────────────────────────

/// Entry: (word_id, chunk_index, total_chunks).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkWordEntry {
    pub word_id: u32,
    pub chunk_index: u8,
    pub total_chunks: u8,
}

/// Builder for ChunkWordMap.
pub struct ChunkWordMapWriter {
    /// For each content ordinal: list of word entries.
    groups: Vec<Vec<ChunkWordEntry>>,
}

impl ChunkWordMapWriter {
    pub fn new(num_content_ords: usize) -> Self {
        Self {
            groups: vec![Vec::new(); num_content_ords],
        }
    }

    /// Record that content ordinal `ord` is chunk `chunk_index` of `total_chunks`
    /// in word `word_id`.
    pub fn add(&mut self, ord: u32, word_id: u32, chunk_index: u8, total_chunks: u8) {
        if (ord as usize) < self.groups.len() {
            let entry = ChunkWordEntry { word_id, chunk_index, total_chunks };
            // Dedup: don't add the same entry twice
            if !self.groups[ord as usize].contains(&entry) {
                self.groups[ord as usize].push(entry);
            }
        }
    }

    /// Serialize to binary.
    /// Format: [4B num_ords] [4B × (num_ords+1) offsets] [6B × N entries]
    pub fn serialize(&self) -> Vec<u8> {
        let num = self.groups.len() as u32;
        let mut offsets: Vec<u32> = Vec::with_capacity(self.groups.len() + 1);
        let mut entries: Vec<u8> = Vec::new();

        for group in &self.groups {
            offsets.push(entries.len() as u32);
            for e in group {
                entries.extend_from_slice(&e.word_id.to_le_bytes());
                entries.push(e.chunk_index);
                entries.push(e.total_chunks);
            }
        }
        offsets.push(entries.len() as u32);

        let mut buf = Vec::with_capacity(4 + (self.groups.len() + 1) * 4 + entries.len());
        buf.extend_from_slice(&num.to_le_bytes());
        for off in &offsets {
            buf.extend_from_slice(&off.to_le_bytes());
        }
        buf.extend_from_slice(&entries);
        buf
    }
}

/// Reader for ChunkWordMap. O(1) lookup per ordinal.
pub struct ChunkWordMapReader<'a> {
    num_ords: u32,
    offsets: &'a [u8],
    entries: &'a [u8],
}

impl<'a> ChunkWordMapReader<'a> {
    pub fn open(data: &'a [u8]) -> Option<Self> {
        if data.len() < 4 { return None; }
        let num = u32::from_le_bytes(data[0..4].try_into().ok()?);
        let offsets_size = (num as usize + 1) * 4;
        if data.len() < 4 + offsets_size { return None; }
        Some(Self {
            num_ords: num,
            offsets: &data[4..4 + offsets_size],
            entries: &data[4 + offsets_size..],
        })
    }

    /// Get all word entries for a content ordinal.
    /// Number of ordinals covered. Needed to walk the map during a merge, which
    /// has to remap every entry rather than look one up.
    pub fn num_ords(&self) -> u32 { self.num_ords }

    pub fn lookup(&self, ord: u32) -> Vec<ChunkWordEntry> {
        if ord >= self.num_ords { return Vec::new(); }
        let start = self.read_offset(ord) as usize;
        let end = self.read_offset(ord + 1) as usize;
        if start >= end || start >= self.entries.len() { return Vec::new(); }
        let slice = &self.entries[start..end.min(self.entries.len())];
        slice.chunks_exact(6)
            .map(|b| ChunkWordEntry {
                word_id: u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
                chunk_index: b[4],
                total_chunks: b[5],
            })
            .collect()
    }

    fn read_offset(&self, idx: u32) -> u32 {
        let pos = idx as usize * 4;
        u32::from_le_bytes(self.offsets[pos..pos + 4].try_into().unwrap())
    }
}

// ─── NextWordMap ───────────────────────────────────────────────────────

/// Builder for NextWordMap.
pub struct NextWordMapWriter {
    /// For each word_id: set of next word_ids.
    groups: Vec<Vec<u32>>,
}

impl NextWordMapWriter {
    pub fn new(num_words: usize) -> Self {
        Self {
            groups: vec![Vec::new(); num_words],
        }
    }

    /// Grow to accommodate word_id if needed.
    fn ensure_capacity(&mut self, word_id: u32) {
        let needed = word_id as usize + 1;
        if needed > self.groups.len() {
            self.groups.resize(needed, Vec::new());
        }
    }

    /// Record that word `next` can follow word `prev`.
    pub fn add(&mut self, prev: u32, next: u32) {
        self.ensure_capacity(prev);
        let group = &mut self.groups[prev as usize];
        if !group.contains(&next) {
            group.push(next);
        }
    }

    /// Serialize to binary.
    /// Format: [4B num_words] [4B × (num_words+1) offsets] [4B × N entries]
    pub fn serialize(&self) -> Vec<u8> {
        let num = self.groups.len() as u32;
        let mut offsets: Vec<u32> = Vec::with_capacity(self.groups.len() + 1);
        let mut entries: Vec<u8> = Vec::new();

        for group in &self.groups {
            offsets.push(entries.len() as u32);
            for &next in group {
                entries.extend_from_slice(&next.to_le_bytes());
            }
        }
        offsets.push(entries.len() as u32);

        let mut buf = Vec::with_capacity(4 + (self.groups.len() + 1) * 4 + entries.len());
        buf.extend_from_slice(&num.to_le_bytes());
        for off in &offsets {
            buf.extend_from_slice(&off.to_le_bytes());
        }
        buf.extend_from_slice(&entries);
        buf
    }
}

/// Reader for NextWordMap. O(1) lookup per word_id.
pub struct NextWordMapReader<'a> {
    num_words: u32,
    offsets: &'a [u8],
    entries: &'a [u8],
}

impl<'a> NextWordMapReader<'a> {
    pub fn open(data: &'a [u8]) -> Option<Self> {
        if data.len() < 4 { return None; }
        let num = u32::from_le_bytes(data[0..4].try_into().ok()?);
        let offsets_size = (num as usize + 1) * 4;
        if data.len() < 4 + offsets_size { return None; }
        Some(Self {
            num_words: num,
            offsets: &data[4..4 + offsets_size],
            entries: &data[4 + offsets_size..],
        })
    }

    /// Check if word `next` can follow word `prev`.
    pub fn contains(&self, prev: u32, next: u32) -> bool {
        self.next_words(prev).contains(&next)
    }

    /// Get all words that can follow `word_id`.
    /// Number of words covered — see ChunkWordMapReader::num_ords.
    pub fn num_words(&self) -> u32 { self.num_words }

    pub fn next_words(&self, word_id: u32) -> Vec<u32> {
        if word_id >= self.num_words { return Vec::new(); }
        let start = self.read_offset(word_id) as usize;
        let end = self.read_offset(word_id + 1) as usize;
        if start >= end || start >= self.entries.len() { return Vec::new(); }
        let slice = &self.entries[start..end.min(self.entries.len())];
        slice.chunks_exact(4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect()
    }

    fn read_offset(&self, idx: u32) -> u32 {
        let pos = idx as usize * 4;
        u32::from_le_bytes(self.offsets[pos..pos + 4].try_into().unwrap())
    }
}

// ─── Chain verification ────────────────────────────────────────────────

/// Verify that a cross-token chain match is structurally valid.
///
/// For each pair of adjacent positions (ordA, ordB), checks:
/// - Intra-word: both ordinals are chunks of the same word at consecutive indices
/// - Inter-word: ordA is the last chunk of its word, ordB is the first chunk
///   of a word that follows ordA's word in the corpus
///
/// Returns true if ANY valid combination exists.
pub fn verify_chain_adjacency(
    chunk_map: &ChunkWordMapReader<'_>,
    next_map: &NextWordMapReader<'_>,
    ord_a: u64,
    ord_b: u64,
) -> bool {
    let entries_a = chunk_map.lookup(ord_a as u32);
    let entries_b = chunk_map.lookup(ord_b as u32);

    if entries_a.is_empty() || entries_b.is_empty() {
        return true; // no word map data → don't filter (backwards compat)
    }

    entries_a.iter().any(|a| {
        entries_b.iter().any(|b| {
            // Intra-word: consecutive chunks of the same word
            (a.word_id == b.word_id && b.chunk_index == a.chunk_index + 1)
            ||
            // Inter-word: last chunk of word A → first chunk of word B
            (a.chunk_index == a.total_chunks - 1
                && b.chunk_index == 0
                && next_map.contains(a.word_id, b.word_id))
        })
    })
}

// ─── SfxIndexFile implementations ──────────────────────────────────────

use super::index_registry::{SfxIndexFile, MergeStrategy};

/// Registry entry for ChunkWordMap.
pub struct ChunkWordMapIndex;

impl SfxIndexFile for ChunkWordMapIndex {
    fn id(&self) -> &'static str { "chunk_word_map" }
    fn extension(&self) -> &'static str { "chunk_word_map" }
    fn merge_strategy(&self) -> MergeStrategy { MergeStrategy::OrMergeWithRemap }
    fn prebuilt_by_collector(&self) -> bool { true }
}

/// Registry entry for NextWordMap.
pub struct NextWordMapIndex;

impl SfxIndexFile for NextWordMapIndex {
    fn id(&self) -> &'static str { "next_word_map" }
    fn extension(&self) -> &'static str { "next_word_map" }
    fn merge_strategy(&self) -> MergeStrategy { MergeStrategy::OrMergeWithRemap }
    fn prebuilt_by_collector(&self) -> bool { true }
}

// ─── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chunk_word_map_roundtrip() {
        let mut writer = ChunkWordMapWriter::new(3);
        writer.add(0, 10, 0, 2); // ord 0 = chunk 0/2 of word 10
        writer.add(0, 20, 1, 3); // ord 0 = chunk 1/3 of word 20
        writer.add(1, 10, 1, 2); // ord 1 = chunk 1/2 of word 10
        writer.add(2, 30, 0, 1); // ord 2 = chunk 0/1 of word 30

        let data = writer.serialize();
        let reader = ChunkWordMapReader::open(&data).unwrap();

        let e0 = reader.lookup(0);
        assert_eq!(e0.len(), 2);
        assert_eq!(e0[0], ChunkWordEntry { word_id: 10, chunk_index: 0, total_chunks: 2 });
        assert_eq!(e0[1], ChunkWordEntry { word_id: 20, chunk_index: 1, total_chunks: 3 });

        let e1 = reader.lookup(1);
        assert_eq!(e1.len(), 1);
        assert_eq!(e1[0].word_id, 10);

        assert!(reader.lookup(99).is_empty());
    }

    #[test]
    fn test_next_word_map_roundtrip() {
        let mut writer = NextWordMapWriter::new(3);
        writer.add(0, 1); // word 0 → word 1
        writer.add(0, 2); // word 0 → word 2
        writer.add(1, 2); // word 1 → word 2

        let data = writer.serialize();
        let reader = NextWordMapReader::open(&data).unwrap();

        assert!(reader.contains(0, 1));
        assert!(reader.contains(0, 2));
        assert!(reader.contains(1, 2));
        assert!(!reader.contains(1, 0));
        assert!(!reader.contains(2, 0));
    }

    #[test]
    fn test_verify_intra_word() {
        let mut cw = ChunkWordMapWriter::new(2);
        cw.add(0, 10, 0, 2); // ord 0 = chunk 0 of word 10 (2 chunks)
        cw.add(1, 10, 1, 2); // ord 1 = chunk 1 of word 10

        let nw = NextWordMapWriter::new(1);

        let cw_data = cw.serialize();
        let nw_data = nw.serialize();
        let cw_r = ChunkWordMapReader::open(&cw_data).unwrap();
        let nw_r = NextWordMapReader::open(&nw_data).unwrap();

        assert!(verify_chain_adjacency(&cw_r, &nw_r, 0, 1)); // chunk 0 → chunk 1 of same word
        assert!(!verify_chain_adjacency(&cw_r, &nw_r, 1, 0)); // chunk 1 → chunk 0 = wrong order
    }

    #[test]
    fn test_verify_inter_word() {
        let mut cw = ChunkWordMapWriter::new(3);
        cw.add(0, 10, 0, 1); // ord 0 = only chunk of word 10
        cw.add(1, 20, 0, 1); // ord 1 = only chunk of word 20
        cw.add(2, 30, 0, 1); // ord 2 = only chunk of word 30

        let mut nw = NextWordMapWriter::new(3);
        nw.add(10, 20); // word 10 → word 20

        let cw_data = cw.serialize();
        let nw_data = nw.serialize();
        let cw_r = ChunkWordMapReader::open(&cw_data).unwrap();
        let nw_r = NextWordMapReader::open(&nw_data).unwrap();

        assert!(verify_chain_adjacency(&cw_r, &nw_r, 0, 1)); // word 10 → word 20 OK
        assert!(!verify_chain_adjacency(&cw_r, &nw_r, 0, 2)); // word 10 → word 30 not in next_map
        assert!(!verify_chain_adjacency(&cw_r, &nw_r, 1, 0)); // word 20 → word 10 not in next_map
    }

    #[test]
    fn test_dedup() {
        let mut cw = ChunkWordMapWriter::new(1);
        cw.add(0, 10, 0, 2);
        cw.add(0, 10, 0, 2); // duplicate
        cw.add(0, 10, 0, 2); // duplicate

        let data = cw.serialize();
        let reader = ChunkWordMapReader::open(&data).unwrap();
        assert_eq!(reader.lookup(0).len(), 1); // deduped
    }
}
