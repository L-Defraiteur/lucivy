//! Position-to-ordinal map: for each (doc_id, position) → ordinal.
//!
//! The reverse of the posting index. Enables O(1) lookup of which token
//! ordinal sits at a given position in a given document. Used by regex
//! cross-token search to validate the path between two known literal
//! positions without sibling walk.
//!
//! Format:
//! ```text
//! [4 bytes] magic "PMAP"
//! [4 bytes] num_docs: u32 LE
//! [8 bytes × (num_docs + 1)] offset table (byte offset into data section)
//! Data section (per doc):
//!   [4 bytes × num_tokens] ordinals: u32 LE, one per position
//! ```

/// Builds a position-to-ordinal map during indexation.
pub struct PosMapWriter {
    /// Per doc: ordinals in position order. Index = doc_id.
    docs: Vec<Vec<u32>>,
}

impl PosMapWriter {
    pub fn new() -> Self {
        Self { docs: Vec::new() }
    }

    /// Record that `ordinal` appears at `position` in `doc_id`.
    pub fn add(&mut self, doc_id: u32, position: u32, ordinal: u32) {
        let d = doc_id as usize;
        if d >= self.docs.len() {
            self.docs.resize(d + 1, Vec::new());
        }
        let doc = &mut self.docs[d];
        let p = position as usize;
        if p >= doc.len() {
            doc.resize(p + 1, u32::MAX);
        }
        doc[p] = ordinal;
    }

    /// Add an empty doc (no tokens).
    pub fn add_empty_doc(&mut self) {
        self.docs.push(Vec::new());
    }

    /// Serialize to binary format.
    pub fn serialize(&self) -> Vec<u8> {
        let num_docs = self.docs.len() as u32;
        let header_size = 4 + 4 + (num_docs as usize + 1) * 8; // magic + num_docs + offsets
        let data_size: usize = self.docs.iter().map(|d| d.len() * 4).sum();
        let mut buf = Vec::with_capacity(header_size + data_size);

        // Magic
        buf.extend_from_slice(b"PMAP");
        buf.extend_from_slice(&num_docs.to_le_bytes());

        // Offset table
        let mut offset: u64 = 0;
        for doc in &self.docs {
            buf.extend_from_slice(&offset.to_le_bytes());
            offset += (doc.len() * 4) as u64;
        }
        buf.extend_from_slice(&offset.to_le_bytes()); // sentinel

        // Data
        for doc in &self.docs {
            for &ord in doc {
                buf.extend_from_slice(&ord.to_le_bytes());
            }
        }

        buf
    }
}

/// Reads a position-to-ordinal map. O(1) lookup.
pub struct PosMapReader<'a> {
    num_docs: u32,
    offsets: &'a [u8],
    data: &'a [u8],
}

impl<'a> PosMapReader<'a> {
    /// Open from raw bytes. Returns None if data is too small or invalid magic.
    pub fn open(bytes: &'a [u8]) -> Option<Self> {
        if bytes.len() < 8 || &bytes[0..4] != b"PMAP" {
            return None;
        }
        let num_docs = u32::from_le_bytes(bytes[4..8].try_into().ok()?);
        let offsets_size = (num_docs as usize + 1) * 8;
        if bytes.len() < 8 + offsets_size {
            return None;
        }
        let offsets = &bytes[8..8 + offsets_size];
        let data = &bytes[8 + offsets_size..];
        Some(Self { num_docs, offsets, data })
    }

    /// Get the ordinal at (doc_id, position). Returns None if out of bounds.
    pub fn ordinal_at(&self, doc_id: u32, position: u32) -> Option<u32> {
        if doc_id >= self.num_docs {
            return None;
        }
        let start = self.read_offset(doc_id) as usize;
        let end = self.read_offset(doc_id + 1) as usize;
        let doc_data = &self.data[start..end.min(self.data.len())];
        let num_tokens = doc_data.len() / 4;
        let p = position as usize;
        if p >= num_tokens {
            return None;
        }
        let off = p * 4;
        let ord = u32::from_le_bytes(doc_data[off..off + 4].try_into().ok()?);
        if ord == u32::MAX {
            None // unfilled position
        } else {
            Some(ord)
        }
    }

    /// Get ordinals for a range of positions [pos_from, pos_to) in a doc.
    /// Returns Vec of (position, ordinal) pairs for valid positions.
    pub fn ordinals_range(&self, doc_id: u32, pos_from: u32, pos_to: u32) -> Vec<(u32, u32)> {
        if doc_id >= self.num_docs {
            return Vec::new();
        }
        let start = self.read_offset(doc_id) as usize;
        let end = self.read_offset(doc_id + 1) as usize;
        let doc_data = &self.data[start..end.min(self.data.len())];
        let num_tokens = doc_data.len() / 4;

        let mut result = Vec::new();
        for pos in pos_from..pos_to.min(num_tokens as u32) {
            let off = pos as usize * 4;
            if off + 4 <= doc_data.len() {
                let ord = u32::from_le_bytes(doc_data[off..off + 4].try_into().unwrap());
                if ord != u32::MAX {
                    result.push((pos, ord));
                }
            }
        }
        result
    }

    /// Number of tokens in a document.
    pub fn num_tokens(&self, doc_id: u32) -> u32 {
        if doc_id >= self.num_docs {
            return 0;
        }
        let start = self.read_offset(doc_id) as usize;
        let end = self.read_offset(doc_id + 1) as usize;
        ((end - start) / 4) as u32
    }

    fn read_offset(&self, idx: u32) -> u64 {
        let pos = idx as usize * 8;
        u64::from_le_bytes(self.offsets[pos..pos + 8].try_into().unwrap())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_posmap_roundtrip() {
        let mut writer = PosMapWriter::new();
        // Doc 0: 3 tokens
        writer.add(0, 0, 10); // pos 0 = ordinal 10
        writer.add(0, 1, 20); // pos 1 = ordinal 20
        writer.add(0, 2, 30); // pos 2 = ordinal 30
        // Doc 1: 2 tokens
        writer.add(1, 0, 5);
        writer.add(1, 1, 15);

        let data = writer.serialize();
        let reader = PosMapReader::open(&data).unwrap();

        assert_eq!(reader.ordinal_at(0, 0), Some(10));
        assert_eq!(reader.ordinal_at(0, 1), Some(20));
        assert_eq!(reader.ordinal_at(0, 2), Some(30));
        assert_eq!(reader.ordinal_at(0, 3), None); // out of bounds
        assert_eq!(reader.ordinal_at(1, 0), Some(5));
        assert_eq!(reader.ordinal_at(1, 1), Some(15));
        assert_eq!(reader.ordinal_at(2, 0), None); // no doc 2

        let range = reader.ordinals_range(0, 1, 3);
        assert_eq!(range, vec![(1, 20), (2, 30)]);
    }

    #[test]
    fn test_posmap_empty_doc() {
        let mut writer = PosMapWriter::new();
        writer.add(0, 0, 42);
        writer.add_empty_doc(); // doc 1 is empty
        writer.add(2, 0, 99);

        let data = writer.serialize();
        let reader = PosMapReader::open(&data).unwrap();

        assert_eq!(reader.ordinal_at(0, 0), Some(42));
        assert_eq!(reader.num_tokens(1), 0);
        assert_eq!(reader.ordinal_at(2, 0), Some(99));
    }
}

// ─────────────────────────────────────────────────────────────────────
// SfxIndexFile implementation (Derived)
// ─────────────────────────────────────────────────────────────────────

pub struct PosMapIndex {
    writer: PosMapWriter,
}

impl PosMapIndex {
    pub fn new() -> Self { Self { writer: PosMapWriter::new() } }
}

impl super::index_registry::SfxIndexFile for PosMapIndex {
    fn id(&self) -> &'static str { "posmap" }
    fn extension(&self) -> &'static str { "posmap" }
    fn kind(&self) -> super::index_registry::IndexKind { super::index_registry::IndexKind::Derived }

    fn on_posting(&mut self, ord: u32, doc_id: u32, position: u32, _bf: u32, _bt: u32) {
        self.writer.add(doc_id, position, ord);
    }

    fn serialize(&self) -> Vec<u8> { self.writer.serialize() }
}
