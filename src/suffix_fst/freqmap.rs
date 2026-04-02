//! FreqMap index: doc_freq and term_freq for BM25 scoring via SFX.
//!
//! Provides O(1) doc_freq(ordinal) and O(log n) term_freq(ordinal, doc_id)
//! lookups, enabling BM25 scoring without the tantivy term dict.
//!
//! Format:
//! ```text
//! [4 bytes] magic "FREQ"
//! [4 bytes] num_terms: u32 LE
//! [4 bytes × num_terms] doc_freq per ordinal
//! [4 bytes × (num_terms + 1)] offset table into tf_data
//! [tf_data] per ordinal: (doc_id: u32 LE, tf: u32 LE) × doc_freq, sorted by doc_id
//! ```

use std::collections::HashMap;

const MAGIC: &[u8; 4] = b"FREQ";

// ─────────────────────────────────────────────────────────────────────
// Writer
// ─────────────────────────────────────────────────────────────────────

pub struct FreqMapWriter {
    /// (ord, doc_id) → term_freq
    freqs: HashMap<(u32, u32), u32>,
    max_ord: u32,
}

impl FreqMapWriter {
    pub fn new() -> Self {
        Self { freqs: HashMap::new(), max_ord: 0 }
    }

    pub fn add(&mut self, ord: u32, doc_id: u32) {
        *self.freqs.entry((ord, doc_id)).or_insert(0) += 1;
        if ord >= self.max_ord { self.max_ord = ord + 1; }
    }

    pub fn serialize(&self) -> Vec<u8> {
        let num_terms = self.max_ord;
        if num_terms == 0 {
            return Vec::new();
        }

        // Group by ordinal, sort entries by doc_id
        let mut per_ord: Vec<Vec<(u32, u32)>> = vec![Vec::new(); num_terms as usize];
        for (&(ord, doc_id), &tf) in &self.freqs {
            if ord < num_terms {
                per_ord[ord as usize].push((doc_id, tf));
            }
        }
        for entries in &mut per_ord {
            entries.sort_by_key(|&(doc_id, _)| doc_id);
        }

        // Compute sizes
        let doc_freq_size = num_terms as usize * 4;
        let offset_table_size = (num_terms as usize + 1) * 4;
        let total_entries: usize = per_ord.iter().map(|e| e.len()).sum();
        let tf_data_size = total_entries * 8; // (doc_id: u32, tf: u32) per entry
        let header_size = 4 + 4; // magic + num_terms

        let mut buf = Vec::with_capacity(header_size + doc_freq_size + offset_table_size + tf_data_size);

        // Header
        buf.extend_from_slice(MAGIC);
        buf.extend_from_slice(&num_terms.to_le_bytes());

        // Doc freq per ordinal
        for entries in &per_ord {
            buf.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        }

        // Offset table
        let mut offset: u32 = 0;
        for entries in &per_ord {
            buf.extend_from_slice(&offset.to_le_bytes());
            offset += (entries.len() as u32) * 8;
        }
        buf.extend_from_slice(&offset.to_le_bytes()); // sentinel

        // TF data
        for entries in &per_ord {
            for &(doc_id, tf) in entries {
                buf.extend_from_slice(&doc_id.to_le_bytes());
                buf.extend_from_slice(&tf.to_le_bytes());
            }
        }

        buf
    }
}

// ─────────────────────────────────────────────────────────────────────
// Reader
// ─────────────────────────────────────────────────────────────────────

pub struct FreqMapReader<'a> {
    num_terms: u32,
    doc_freqs: &'a [u8],    // u32 × num_terms
    offsets: &'a [u8],       // u32 × (num_terms + 1)
    tf_data: &'a [u8],
}

impl<'a> FreqMapReader<'a> {
    pub fn open(bytes: &'a [u8]) -> Option<Self> {
        if bytes.len() < 8 { return None; }
        if &bytes[0..4] != MAGIC { return None; }
        let num_terms = u32::from_le_bytes(bytes[4..8].try_into().ok()?);

        let df_size = num_terms as usize * 4;
        let ofs_size = (num_terms as usize + 1) * 4;
        let header = 8;
        if bytes.len() < header + df_size + ofs_size { return None; }

        let doc_freqs = &bytes[header..header + df_size];
        let offsets = &bytes[header + df_size..header + df_size + ofs_size];
        let tf_data = &bytes[header + df_size + ofs_size..];

        Some(Self { num_terms, doc_freqs, offsets, tf_data })
    }

    pub fn num_terms(&self) -> u32 { self.num_terms }

    /// Number of documents containing this ordinal. O(1).
    pub fn doc_freq(&self, ordinal: u32) -> u32 {
        if ordinal >= self.num_terms { return 0; }
        let pos = ordinal as usize * 4;
        u32::from_le_bytes(self.doc_freqs[pos..pos + 4].try_into().unwrap())
    }

    /// Term frequency in a specific document. O(log n) via binary search.
    pub fn term_freq(&self, ordinal: u32, doc_id: u32) -> u32 {
        if ordinal >= self.num_terms { return 0; }
        let start = self.read_offset(ordinal) as usize;
        let end = self.read_offset(ordinal + 1) as usize;
        if start >= end || end > self.tf_data.len() { return 0; }

        let entries = &self.tf_data[start..end];
        let n = (end - start) / 8;

        // Binary search on doc_id
        let mut lo = 0usize;
        let mut hi = n;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let mid_doc = u32::from_le_bytes(entries[mid * 8..mid * 8 + 4].try_into().unwrap());
            if mid_doc < doc_id {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        if lo < n {
            let found_doc = u32::from_le_bytes(entries[lo * 8..lo * 8 + 4].try_into().unwrap());
            if found_doc == doc_id {
                return u32::from_le_bytes(entries[lo * 8 + 4..lo * 8 + 8].try_into().unwrap());
            }
        }
        0
    }

    /// Sum of term_freq across all docs for this ordinal.
    pub fn total_term_freq(&self, ordinal: u32) -> u64 {
        if ordinal >= self.num_terms { return 0; }
        let start = self.read_offset(ordinal) as usize;
        let end = self.read_offset(ordinal + 1) as usize;
        if start >= end || end > self.tf_data.len() { return 0; }

        let entries = &self.tf_data[start..end];
        let n = (end - start) / 8;
        let mut total = 0u64;
        for i in 0..n {
            total += u32::from_le_bytes(entries[i * 8 + 4..i * 8 + 8].try_into().unwrap()) as u64;
        }
        total
    }

    fn read_offset(&self, idx: u32) -> u32 {
        let pos = idx as usize * 4;
        u32::from_le_bytes(self.offsets[pos..pos + 4].try_into().unwrap())
    }
}

// ─────────────────────────────────────────────────────────────────────
// SfxIndexFile implementation (Derived)
// ─────────────────────────────────────────────────────────────────────

pub struct FreqMapIndex {
    writer: FreqMapWriter,
}

impl FreqMapIndex {
    pub fn new() -> Self { Self { writer: FreqMapWriter::new() } }
}

impl super::index_registry::SfxIndexFile for FreqMapIndex {
    fn id(&self) -> &'static str { "freqmap" }
    fn extension(&self) -> &'static str { "freqmap" }
    fn merge_strategy(&self) -> super::index_registry::MergeStrategy { super::index_registry::MergeStrategy::EventDriven }

    fn on_posting(&mut self, ord: u32, doc_id: u32, _position: u32, _bf: u32, _bt: u32) {
        self.writer.add(ord, doc_id);
    }

    fn serialize(&self) -> Vec<u8> { self.writer.serialize() }
}

// ─────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_freqmap_roundtrip() {
        let mut writer = FreqMapWriter::new();
        // Token 0 appears in doc 0 (tf=3) and doc 2 (tf=1)
        writer.add(0, 0); writer.add(0, 0); writer.add(0, 0);
        writer.add(0, 2);
        // Token 1 appears in doc 1 (tf=2)
        writer.add(1, 1); writer.add(1, 1);
        // Token 2 appears in doc 0 (tf=1), doc 1 (tf=1), doc 2 (tf=1)
        writer.add(2, 0); writer.add(2, 1); writer.add(2, 2);

        let data = writer.serialize();
        let reader = FreqMapReader::open(&data).unwrap();

        assert_eq!(reader.num_terms(), 3);

        // doc_freq
        assert_eq!(reader.doc_freq(0), 2);  // token 0 in 2 docs
        assert_eq!(reader.doc_freq(1), 1);  // token 1 in 1 doc
        assert_eq!(reader.doc_freq(2), 3);  // token 2 in 3 docs
        assert_eq!(reader.doc_freq(3), 0);  // out of bounds

        // term_freq
        assert_eq!(reader.term_freq(0, 0), 3);
        assert_eq!(reader.term_freq(0, 1), 0);  // token 0 not in doc 1
        assert_eq!(reader.term_freq(0, 2), 1);
        assert_eq!(reader.term_freq(1, 1), 2);
        assert_eq!(reader.term_freq(2, 0), 1);
        assert_eq!(reader.term_freq(2, 1), 1);
        assert_eq!(reader.term_freq(2, 2), 1);

        // total_term_freq
        assert_eq!(reader.total_term_freq(0), 4); // 3+1
        assert_eq!(reader.total_term_freq(1), 2);
        assert_eq!(reader.total_term_freq(2), 3); // 1+1+1
    }

    #[test]
    fn test_freqmap_empty() {
        let writer = FreqMapWriter::new();
        let data = writer.serialize();
        assert!(data.is_empty());
    }

    #[test]
    fn test_freqmap_single_entry() {
        let mut writer = FreqMapWriter::new();
        writer.add(0, 42);
        let data = writer.serialize();
        let reader = FreqMapReader::open(&data).unwrap();
        assert_eq!(reader.doc_freq(0), 1);
        assert_eq!(reader.term_freq(0, 42), 1);
        assert_eq!(reader.term_freq(0, 0), 0);
    }
}
