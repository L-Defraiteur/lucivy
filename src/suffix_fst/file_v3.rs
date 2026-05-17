//! SFX file format v3 — section-based, no sibling table, no gapmap.
//!
//! Uses the section_file container with magic "SFX3".
//!
//! Sections:
//!   0x01 — FST: suffix FST bytes
//!   0x02 — PARENTS: OutputTable bytes (v3 encoding)
//!   0x03 — WORD_MAP: token_to_word[TI]→WI + word_start_token[WI]→TI
//!   0x04 — NEXT_WORD: next_word[TI]→TI (next token that is_word_start)
//!
//! Removed vs v2: sibling table, gapmap, sepmap (all in separate files or gone).

use lucivy_fst::{Map, OutputTable};

use super::builder_v3::{
    decode_output_v3, decode_parent_entries_v3, ParentEntryV3, ParentRefV3,
};
use super::section_file::{SectionFileReader, SectionFileWriter};

const MAGIC: [u8; 4] = *b"SFX3";
const VERSION: u8 = 3;

/// Section IDs for the .sfx v3 file.
pub const SECTION_FST: u16 = 0x01;
pub const SECTION_PARENTS: u16 = 0x02;
pub const SECTION_WORD_MAP: u16 = 0x03;
pub const SECTION_NEXT_WORD: u16 = 0x04;

// ─── Word Map format ───────────────────────────────────────────────────────
//
// SECTION_WORD_MAP:
//   [4 bytes] num_tokens: u32 LE
//   [4 bytes] num_words: u32 LE
//   [num_tokens × 4 bytes] token_to_word: u32 LE (TI → WI)
//   [num_words × 4 bytes] word_start_token: u32 LE (WI → TI)
//   [num_words × 2 bytes] word_content_len: u16 LE (total content bytes per word)
//
// SECTION_NEXT_WORD:
//   [4 bytes] num_tokens: u32 LE
//   [num_tokens × 4 bytes] next_word: u32 LE (TI → TI of next word start, u32::MAX if none)

/// Word map data for BM25 scoring and term/startsWith queries.
#[derive(Debug, Clone)]
pub struct WordMap {
    /// TI → WI: which word contains this token.
    pub token_to_word: Vec<u32>,
    /// WI → TI: first token of each word.
    pub word_start_token: Vec<u32>,
    /// WI → total content bytes (all chunks, no sep).
    pub word_content_len: Vec<u16>,
}

impl WordMap {
    pub fn new() -> Self {
        Self {
            token_to_word: Vec::new(),
            word_start_token: Vec::new(),
            word_content_len: Vec::new(),
        }
    }

    fn serialize(&self) -> Vec<u8> {
        let num_tokens = self.token_to_word.len() as u32;
        let num_words = self.word_start_token.len() as u32;
        let size = 8 + num_tokens as usize * 4 + num_words as usize * 4 + num_words as usize * 2;
        let mut buf = Vec::with_capacity(size);

        buf.extend_from_slice(&num_tokens.to_le_bytes());
        buf.extend_from_slice(&num_words.to_le_bytes());
        for &wi in &self.token_to_word {
            buf.extend_from_slice(&wi.to_le_bytes());
        }
        for &ti in &self.word_start_token {
            buf.extend_from_slice(&ti.to_le_bytes());
        }
        for &cl in &self.word_content_len {
            buf.extend_from_slice(&cl.to_le_bytes());
        }
        buf
    }

    fn deserialize(data: &[u8]) -> Option<Self> {
        if data.len() < 8 { return None; }
        let num_tokens = u32::from_le_bytes(data[0..4].try_into().ok()?) as usize;
        let num_words = u32::from_le_bytes(data[4..8].try_into().ok()?) as usize;

        let expected = 8 + num_tokens * 4 + num_words * 4 + num_words * 2;
        if data.len() < expected { return None; }

        let mut pos = 8;
        let mut token_to_word = Vec::with_capacity(num_tokens);
        for _ in 0..num_tokens {
            token_to_word.push(u32::from_le_bytes(data[pos..pos + 4].try_into().ok()?));
            pos += 4;
        }
        let mut word_start_token = Vec::with_capacity(num_words);
        for _ in 0..num_words {
            word_start_token.push(u32::from_le_bytes(data[pos..pos + 4].try_into().ok()?));
            pos += 4;
        }
        let mut word_content_len = Vec::with_capacity(num_words);
        for _ in 0..num_words {
            word_content_len.push(u16::from_le_bytes(data[pos..pos + 2].try_into().ok()?));
            pos += 2;
        }

        Some(Self { token_to_word, word_start_token, word_content_len })
    }
}

impl Default for WordMap {
    fn default() -> Self { Self::new() }
}

/// Serializes next_word table: TI → TI of next word start.
fn serialize_next_word(next_word: &[u32]) -> Vec<u8> {
    let num = next_word.len() as u32;
    let mut buf = Vec::with_capacity(4 + next_word.len() * 4);
    buf.extend_from_slice(&num.to_le_bytes());
    for &ti in next_word {
        buf.extend_from_slice(&ti.to_le_bytes());
    }
    buf
}

fn deserialize_next_word(data: &[u8]) -> Option<Vec<u32>> {
    if data.len() < 4 { return None; }
    let num = u32::from_le_bytes(data[0..4].try_into().ok()?) as usize;
    if data.len() < 4 + num * 4 { return None; }
    let mut result = Vec::with_capacity(num);
    let mut pos = 4;
    for _ in 0..num {
        result.push(u32::from_le_bytes(data[pos..pos + 4].try_into().ok()?));
        pos += 4;
    }
    Some(result)
}

// ─── Writer ────────────────────────────────────────────────────────────────

/// Assembles a .sfx v3 file from pre-built components.
pub struct SfxFileWriterV3 {
    fst_data: Vec<u8>,
    parent_list_data: Vec<u8>,
    word_map: WordMap,
    next_word: Vec<u32>,
    num_docs: u32,
}

impl SfxFileWriterV3 {
    pub fn new(
        fst_data: Vec<u8>,
        parent_list_data: Vec<u8>,
        num_docs: u32,
    ) -> Self {
        Self {
            fst_data,
            parent_list_data,
            word_map: WordMap::new(),
            next_word: Vec::new(),
            num_docs,
        }
    }

    pub fn with_word_map(mut self, word_map: WordMap) -> Self {
        self.word_map = word_map;
        self
    }

    pub fn with_next_word(mut self, next_word: Vec<u32>) -> Self {
        self.next_word = next_word;
        self
    }

    /// Serialize to bytes using the section file format.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut file = SectionFileWriter::new(MAGIC, VERSION);
        file.add_section(SECTION_FST, &self.fst_data);
        file.add_section(SECTION_PARENTS, &self.parent_list_data);
        if !self.word_map.token_to_word.is_empty() {
            file.add_section(SECTION_WORD_MAP, &self.word_map.serialize());
        }
        if !self.next_word.is_empty() {
            file.add_section(SECTION_NEXT_WORD, &serialize_next_word(&self.next_word));
        }
        file.serialize()
    }
}

// ─── Reader ────────────────────────────────────────────────────────────────

/// Error type for SFX v3 file operations.
#[derive(Debug)]
pub enum SfxV3Error {
    InvalidFormat,
    MissingSection(&'static str),
    FstError(String),
}

impl std::fmt::Display for SfxV3Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SfxV3Error::InvalidFormat => write!(f, "invalid SFX3 format"),
            SfxV3Error::MissingSection(s) => write!(f, "missing section: {s}"),
            SfxV3Error::FstError(e) => write!(f, "FST error: {e}"),
        }
    }
}

impl std::error::Error for SfxV3Error {}

/// Reads a .sfx v3 file.
pub struct SfxFileReaderV3 {
    fst: Map<Vec<u8>>,
    parent_list_data: Vec<u8>,
    word_map: Option<WordMap>,
    next_word: Option<Vec<u32>>,
    num_docs: u32,
}

impl SfxFileReaderV3 {
    /// Open from raw bytes.
    pub fn open(data: &[u8]) -> Result<Self, SfxV3Error> {
        let file = SectionFileReader::open(data, &MAGIC)
            .ok_or(SfxV3Error::InvalidFormat)?;

        let fst_bytes = file.get_section(SECTION_FST)
            .ok_or(SfxV3Error::MissingSection("FST"))?;
        let fst = if fst_bytes.is_empty() {
            Map::new(lucivy_fst::MapBuilder::memory().into_inner().unwrap_or_default())
                .map_err(|e| SfxV3Error::FstError(e.to_string()))?
        } else {
            Map::new(fst_bytes.to_vec())
                .map_err(|e| SfxV3Error::FstError(e.to_string()))?
        };

        let parent_list_data = file.get_section(SECTION_PARENTS)
            .ok_or(SfxV3Error::MissingSection("PARENTS"))?
            .to_vec();

        let word_map = file.get_section(SECTION_WORD_MAP)
            .and_then(WordMap::deserialize);

        let next_word = file.get_section(SECTION_NEXT_WORD)
            .and_then(deserialize_next_word);

        // Derive num_docs from word_map or default to 0
        // (actual num_docs is tracked externally by the segment, not in the .sfx file)
        let num_docs = 0;

        Ok(Self { fst, parent_list_data, word_map, next_word, num_docs })
    }

    /// Access the FST.
    pub fn fst(&self) -> &Map<Vec<u8>> {
        &self.fst
    }

    /// Decode parent(s) from a FST output value.
    pub fn decode_parents(&self, value: u64) -> Vec<ParentEntryV3> {
        match decode_output_v3(value) {
            ParentRefV3::Single(entry) => vec![entry],
            ParentRefV3::Multi { offset } => {
                let table = OutputTable::new(&self.parent_list_data);
                let record = table.get(offset);
                decode_parent_entries_v3(record)
            }
        }
    }

    /// Resolve all parents for a suffix string (for testing/debugging).
    pub fn resolve_suffix(&self, suffix: &str) -> Vec<ParentEntryV3> {
        let lower = suffix.to_lowercase();
        let mut results = Vec::new();

        for &prefix in &[super::builder::SI0_PREFIX, super::builder::SI_REST_PREFIX] {
            let mut key = vec![prefix];
            key.extend_from_slice(lower.as_bytes());
            if let Some(val) = self.fst.get(&key) {
                results.extend(self.decode_parents(val));
            }
        }
        results
    }

    /// Word map (if present).
    pub fn word_map(&self) -> Option<&WordMap> {
        self.word_map.as_ref()
    }

    /// Next-word table (if present).
    pub fn next_word(&self) -> Option<&[u32]> {
        self.next_word.as_deref()
    }

    /// Number of entries in the FST.
    pub fn num_suffix_terms(&self) -> usize {
        self.fst.len()
    }
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suffix_fst::builder_v3::SuffixFstBuilderV3;
    use crate::suffix_fst::collector_v3::SfxCollectorV3;

    /// Build a complete .sfx v3 file from text, return the bytes.
    fn build_sfx_v3(texts: &[&str]) -> Vec<u8> {
        let mut collector = SfxCollectorV3::new();
        for text in texts {
            collector.begin_doc();
            collector.add_value(text);
            collector.end_doc();
        }

        let data = collector.into_data();

        // Build FST
        let mut builder = SuffixFstBuilderV3::with_min_suffix_len(data.min_suffix_len);
        for (final_ord, &intern_ord) in data.sorted_indices.iter().enumerate() {
            let text = &data.token_texts[intern_ord as usize];
            let meta = &data.token_meta[intern_ord as usize];
            builder.add_token(
                text,
                final_ord as u64,
                meta.own_len,
                meta.sep_len,
                meta.overlap_len,
                meta.is_word_start,
            );
        }
        let (fst_bytes, output_table) = builder.build().unwrap();

        // Build word map from collector data
        let num_tokens = data.sorted_indices.len();
        let mut token_to_word = vec![0u32; num_tokens];
        let mut word_starts: std::collections::BTreeMap<usize, u32> = std::collections::BTreeMap::new();

        for (final_ord, &intern_ord) in data.sorted_indices.iter().enumerate() {
            let meta = &data.token_meta[intern_ord as usize];
            token_to_word[final_ord] = meta.word_id as u32;
            if meta.is_word_start {
                word_starts.entry(meta.word_id).or_insert(final_ord as u32);
            }
        }

        let num_words = word_starts.len();
        let word_start_token: Vec<u32> = (0..num_words).map(|wi| {
            *word_starts.get(&wi).unwrap_or(&0)
        }).collect();
        let word_content_len = vec![0u16; num_words]; // simplified for test

        let word_map = WordMap {
            token_to_word,
            word_start_token,
            word_content_len,
        };

        // Build next_word
        let mut next_word = vec![u32::MAX; num_tokens];
        for (final_ord, &intern_ord) in data.sorted_indices.iter().enumerate() {
            let meta = &data.token_meta[intern_ord as usize];
            if !meta.is_word_start { continue; }
            // Find next word_start after this ordinal
            // (simplified: in a real impl this is per-document, not per-ordinal)
        }

        let writer = SfxFileWriterV3::new(fst_bytes, output_table, data.num_docs)
            .with_word_map(word_map)
            .with_next_word(next_word);

        writer.to_bytes()
    }

    #[test]
    fn test_write_read_roundtrip() {
        let bytes = build_sfx_v3(&["mutex_lock"]);
        let reader = SfxFileReaderV3::open(&bytes).unwrap();

        assert!(reader.num_suffix_terms() > 0);
        assert!(reader.word_map().is_some());
        assert!(reader.next_word().is_some());
    }

    #[test]
    fn test_resolve_suffix() {
        let bytes = build_sfx_v3(&["mutex_lock"]);
        let reader = SfxFileReaderV3::open(&bytes).unwrap();

        // "mutex_lo" should be findable at SI=0
        let parents = reader.resolve_suffix("mutex_lo");
        assert!(!parents.is_empty(), "should find mutex_lo");
        assert!(parents.iter().any(|p| p.sti == 0 && p.is_word_start));

        // "x_lo" should be findable (cross-boundary via overlap)
        let parents = reader.resolve_suffix("x_lo");
        assert!(!parents.is_empty(), "should find x_lo (overlap trigram)");
    }

    #[test]
    fn test_parent_metadata() {
        let bytes = build_sfx_v3(&["mutex_lock"]);
        let reader = SfxFileReaderV3::open(&bytes).unwrap();

        let parents = reader.resolve_suffix("mutex_lo");
        let p = parents.iter().find(|p| p.sti == 0).unwrap();
        assert_eq!(p.own_len, 6);
        assert_eq!(p.sep_len, 1);
        assert_eq!(p.overlap_len, 2);
        assert!(p.is_word_start);
        assert_eq!(p.content_len(), 5);
    }

    #[test]
    fn test_multi_doc() {
        let bytes = build_sfx_v3(&["mutex_lock", "mutex_core", "hello_world"]);
        let reader = SfxFileReaderV3::open(&bytes).unwrap();

        // "mutex_lo" from doc 0
        assert!(!reader.resolve_suffix("mutex_lo").is_empty());
        // "mutex_co" from doc 1
        assert!(!reader.resolve_suffix("mutex_co").is_empty());
        // "hello_wo" from doc 2
        assert!(!reader.resolve_suffix("hello_wo").is_empty());
    }

    #[test]
    fn test_no_word_map() {
        // Write without word map
        let mut collector = SfxCollectorV3::new();
        collector.begin_doc();
        collector.add_value("test");
        collector.end_doc();
        let data = collector.into_data();

        let mut builder = SuffixFstBuilderV3::with_min_suffix_len(1);
        for (final_ord, &intern_ord) in data.sorted_indices.iter().enumerate() {
            let text = &data.token_texts[intern_ord as usize];
            let meta = &data.token_meta[intern_ord as usize];
            builder.add_token(text, final_ord as u64, meta.own_len, meta.sep_len, meta.overlap_len, meta.is_word_start);
        }
        let (fst_bytes, output_table) = builder.build().unwrap();

        let writer = SfxFileWriterV3::new(fst_bytes, output_table, 1);
        let bytes = writer.to_bytes();

        let reader = SfxFileReaderV3::open(&bytes).unwrap();
        assert!(reader.word_map().is_none());
        assert!(reader.next_word().is_none());
        assert!(!reader.resolve_suffix("test").is_empty());
    }

    #[test]
    fn test_word_map_roundtrip() {
        let wm = WordMap {
            token_to_word: vec![0, 0, 1, 2],
            word_start_token: vec![0, 2, 3],
            word_content_len: vec![10, 5, 4],
        };
        let data = wm.serialize();
        let wm2 = WordMap::deserialize(&data).unwrap();
        assert_eq!(wm2.token_to_word, vec![0, 0, 1, 2]);
        assert_eq!(wm2.word_start_token, vec![0, 2, 3]);
        assert_eq!(wm2.word_content_len, vec![10, 5, 4]);
    }

    #[test]
    fn test_next_word_roundtrip() {
        let nw = vec![2, 2, 3, u32::MAX];
        let data = serialize_next_word(&nw);
        let nw2 = deserialize_next_word(&data).unwrap();
        assert_eq!(nw2, nw);
    }

    #[test]
    fn test_empty_sfx() {
        let writer = SfxFileWriterV3::new(
            lucivy_fst::MapBuilder::memory().into_inner().unwrap(),
            Vec::new(),
            0,
        );
        let bytes = writer.to_bytes();
        let reader = SfxFileReaderV3::open(&bytes).unwrap();
        assert_eq!(reader.num_suffix_terms(), 0);
    }
}
