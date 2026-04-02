//! Term texts index: O(1) lookup from SFX ordinal → token text.
//!
//! Fixes the ordinal mismatch bug: the tantivy term dictionary has its own
//! ordinals that do NOT match SFX ordinals. This file stores token texts
//! indexed by SFX ordinal, so cross-token search can resolve ordinals
//! without going through the tantivy term dict.
//!
//! Format:
//! ```text
//! [4 bytes] magic "TTXT"
//! [4 bytes] num_terms: u32 LE
//! [4 bytes × (num_terms + 1)] offset table: u32 LE (byte offset into data)
//! [data] concatenated term texts, UTF-8
//! ```

const MAGIC: &[u8; 4] = b"TTXT";

// ─────────────────────────────────────────────────────────────────────
// Writer
// ─────────────────────────────────────────────────────────────────────

pub struct TermTextsWriter {
    texts: Vec<Vec<u8>>,
}

impl TermTextsWriter {
    pub fn new() -> Self {
        Self { texts: Vec::new() }
    }

    pub fn add(&mut self, ordinal: u32, text: &str) {
        let ord = ordinal as usize;
        if ord >= self.texts.len() {
            self.texts.resize(ord + 1, Vec::new());
        }
        self.texts[ord] = text.as_bytes().to_vec();
    }

    pub fn serialize(&self) -> Vec<u8> {
        let num_terms = self.texts.len() as u32;
        let header_size = 4 + 4 + (num_terms as usize + 1) * 4;
        let data_size: usize = self.texts.iter().map(|t| t.len()).sum();
        let mut buf = Vec::with_capacity(header_size + data_size);

        // Magic + num_terms
        buf.extend_from_slice(MAGIC);
        buf.extend_from_slice(&num_terms.to_le_bytes());

        // Offset table
        let mut offset: u32 = 0;
        for text in &self.texts {
            buf.extend_from_slice(&offset.to_le_bytes());
            offset += text.len() as u32;
        }
        buf.extend_from_slice(&offset.to_le_bytes()); // sentinel

        // Data
        for text in &self.texts {
            buf.extend_from_slice(text);
        }

        buf
    }
}

// ─────────────────────────────────────────────────────────────────────
// Reader
// ─────────────────────────────────────────────────────────────────────

pub struct TermTextsReader<'a> {
    num_terms: u32,
    offsets: &'a [u8],
    data: &'a [u8],
}

impl<'a> TermTextsReader<'a> {
    pub fn open(bytes: &'a [u8]) -> Option<Self> {
        if bytes.len() < 8 { return None; }
        if &bytes[0..4] != MAGIC { return None; }
        let num_terms = u32::from_le_bytes(bytes[4..8].try_into().ok()?);
        let offsets_size = (num_terms as usize + 1) * 4;
        if bytes.len() < 8 + offsets_size { return None; }
        let offsets = &bytes[8..8 + offsets_size];
        let data = &bytes[8 + offsets_size..];
        Some(Self { num_terms, offsets, data })
    }

    /// Get the text for an SFX ordinal. O(1).
    pub fn text(&self, ordinal: u32) -> Option<&'a str> {
        if ordinal >= self.num_terms { return None; }
        let start = self.read_offset(ordinal) as usize;
        let end = self.read_offset(ordinal + 1) as usize;
        if end > self.data.len() { return None; }
        std::str::from_utf8(&self.data[start..end]).ok()
    }

    pub fn num_terms(&self) -> u32 {
        self.num_terms
    }

    fn read_offset(&self, idx: u32) -> u32 {
        let pos = idx as usize * 4;
        u32::from_le_bytes(self.offsets[pos..pos + 4].try_into().unwrap())
    }
}

// ─────────────────────────────────────────────────────────────────────
// SfxIndexFile implementation
// ─────────────────────────────────────────────────────────────────────

pub struct TermTextsIndex {
    writer: TermTextsWriter,
}

impl TermTextsIndex {
    pub fn new() -> Self { Self { writer: TermTextsWriter::new() } }
}

impl super::index_registry::SfxIndexFile for TermTextsIndex {
    fn id(&self) -> &'static str { "termtexts" }
    fn extension(&self) -> &'static str { "termtexts" }
    fn kind(&self) -> super::index_registry::IndexKind { super::index_registry::IndexKind::Derived }

    fn on_token(&mut self, ord: u32, text: &str) {
        self.writer.add(ord, text);
    }

    fn serialize(&self) -> Vec<u8> { self.writer.serialize() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_termtexts_roundtrip() {
        let mut writer = TermTextsWriter::new();
        writer.add(0, "hello");
        writer.add(1, "rag3");
        writer.add(2, "weaver");
        writer.add(3, "rag3weaver");

        let data = writer.serialize();
        let reader = TermTextsReader::open(&data).unwrap();

        assert_eq!(reader.num_terms(), 4);
        assert_eq!(reader.text(0), Some("hello"));
        assert_eq!(reader.text(1), Some("rag3"));
        assert_eq!(reader.text(2), Some("weaver"));
        assert_eq!(reader.text(3), Some("rag3weaver"));
        assert_eq!(reader.text(4), None);
    }

    #[test]
    fn test_termtexts_empty() {
        let writer = TermTextsWriter::new();
        let data = writer.serialize();
        let reader = TermTextsReader::open(&data).unwrap();
        assert_eq!(reader.num_terms(), 0);
        assert_eq!(reader.text(0), None);
    }
}
