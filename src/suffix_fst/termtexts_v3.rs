//! Term texts v3 — extended token texts + metadata for merge support.
//!
//! Uses the section_file format with magic "TTX3".
//!
//! Sections:
//!   0x01 — TEXTS: offset table + concatenated UTF-8 texts (same as v2 TTXT)
//!   0x02 — META:  per-ordinal metadata array (own_len, sep_len, overlap_len,
//!                is_word_start, is_word_stripped)
//!   0x03 — STATS: segment-wide facts derived from META at write time
//!                (max word-stripped content length). Optional: files
//!                written before it read back as "unknown", which every
//!                consumer must treat as the pessimistic answer.
//!
//! The texts are the EXTENDED tokens (e.g., "mutex_lo" not "mutex_").
//! The metadata allows the merge process to re-feed tokens to the builder
//! without re-tokenizing.

use super::section_file::{SectionFileReader, SectionFileWriter};

const MAGIC: [u8; 4] = *b"TTX3";
const VERSION: u8 = 1;

const SECTION_TEXTS: u16 = 0x01;
const SECTION_META: u16 = 0x02;
const SECTION_STATS: u16 = 0x03;
/// STATS layout version — see `serialize`.
const STATS_VERSION: u16 = 1;

/// Suffix indexes a word-stripped entry carries (SI 0..=MAX). A match that
/// starts deeper inside a word is only reachable through the chunk chains.
/// Mirrors `builder_v3::MAX_CHUNK_BYTES` and the collector's MAX_SUFFIX_INDEX.
pub const WORD_SUFFIX_CAP: u16 = 256;

/// Per-ordinal metadata stored alongside the token text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TermMetaV3 {
    /// Bytes owned by the token: content + trailing separators, no overlap.
    pub own_len: u16,
    /// Trailing separator bytes included in `own_len`.
    pub sep_len: u8,
    /// Bytes borrowed from the next token at the end of the extended text.
    pub overlap_len: u8,
    /// True when the token is the first chunk of a word.
    pub is_word_start: bool,
    /// Which half of the model this ordinal belongs to: chunk (partitions
    /// 0x00/0x01, postings in `.sfxpost`, chunk-level coordinates) or
    /// word-stripped (partition 0x02, postings in `.word_sfxpost`, word-level).
    ///
    /// The collector separates the two by prefixing intern keys — but those
    /// prefixes only ever existed in memory. Persisting the tag is what lets a
    /// reader rebuild the partition instead of guessing; without it, any structure
    /// keyed on text alone silently fuses the two.
    pub is_word_stripped: bool,
}

// ─── Writer ────────────────────────────────────────────────────────────────

/// Builds a v3 term texts file with metadata.
pub struct TermTextsWriterV3 {
    texts: Vec<Vec<u8>>,
    metas: Vec<TermMetaV3>,
}

impl Default for TermTextsWriterV3 {
    fn default() -> Self {
        Self::new()
    }
}

impl TermTextsWriterV3 {
    /// Empty writer; ordinals are filled in by `add`.
    pub fn new() -> Self {
        Self {
            texts: Vec::new(),
            metas: Vec::new(),
        }
    }

    /// Add an extended token at the given ordinal with its metadata.
    /// A writer holding every token of a collected segment, keyed by final
    /// ordinal — the same loop the assemble node and the test helpers ran
    /// each on their own.
    pub fn from_collector_v3(data: &crate::suffix_fst::collector_v3::SfxCollectorDataV3) -> Self {
        let mut w = Self::new();
        for &intern_ord in &data.sorted_indices {
            let meta = &data.token_meta[intern_ord as usize];
            let text = &data.token_texts[intern_ord as usize];
            let final_ord = data.intern_to_final[intern_ord as usize];
            w.add(final_ord, text, TermMetaV3 {
                own_len: meta.own_len,
                sep_len: meta.sep_len,
                overlap_len: meta.overlap_len,
                is_word_start: meta.is_word_start,
                is_word_stripped: meta.is_word_stripped,
            });
        }
        w
    }

    pub fn add(&mut self, ordinal: u32, text: &str, meta: TermMetaV3) {
        let ord = ordinal as usize;
        if ord >= self.texts.len() {
            self.texts.resize(ord + 1, Vec::new());
            self.metas.resize(ord + 1, TermMetaV3 {
                own_len: 0, sep_len: 0, overlap_len: 0, is_word_start: false, is_word_stripped: false });
        }
        self.texts[ord] = text.as_bytes().to_vec();
        self.metas[ord] = meta;
    }

    /// Serialize to bytes using the section file format.
    pub fn serialize(&self) -> Vec<u8> {
        let mut file = SectionFileWriter::new(MAGIC, VERSION);

        // Section TEXTS: offset table + concatenated texts
        file.add_section(SECTION_TEXTS, &self.serialize_texts());

        // Section META: packed metadata array
        file.add_section(SECTION_META, &self.serialize_meta());

        // Section STATS: max word-stripped content length (u16), then the
        // STATS layout version (u16). The version says what the word
        // partition is complete for: a reader that finds an older layout
        // must not trust `max_word` to skip the chunk chains.
        let max_word: u16 = self.metas.iter()
            .filter(|m| m.is_word_stripped)
            .map(|m| m.own_len.saturating_sub(m.sep_len as u16))
            .max()
            .unwrap_or(0);
        let mut stats = Vec::with_capacity(4);
        stats.extend_from_slice(&max_word.to_le_bytes());
        stats.extend_from_slice(&STATS_VERSION.to_le_bytes());
        file.add_section(SECTION_STATS, &stats);

        file.serialize()
    }

    fn serialize_texts(&self) -> Vec<u8> {
        let num = self.texts.len() as u32;
        let data_size: usize = self.texts.iter().map(|t| t.len()).sum();
        let mut buf = Vec::with_capacity(4 + (num as usize + 1) * 4 + data_size);

        buf.extend_from_slice(&num.to_le_bytes());

        // Offset table
        let mut offset: u32 = 0;
        for text in &self.texts {
            buf.extend_from_slice(&offset.to_le_bytes());
            offset += text.len() as u32;
        }
        buf.extend_from_slice(&offset.to_le_bytes()); // sentinel

        // Text data
        for text in &self.texts {
            buf.extend_from_slice(text);
        }

        buf
    }

    fn serialize_meta(&self) -> Vec<u8> {
        // 6 bytes/entry: own_len(2) sep_len(1) overlap_len(1) is_word_start(1) is_word_stripped(1)
        let num = self.metas.len() as u32;
        let mut buf = Vec::with_capacity(4 + self.metas.len() * 6);
        buf.extend_from_slice(&num.to_le_bytes());
        for m in &self.metas {
            buf.extend_from_slice(&m.own_len.to_le_bytes());
            buf.push(m.sep_len);
            buf.push(m.overlap_len);
            buf.push(if m.is_word_start { 1 } else { 0 });
            // Was `reserved`. Old files carry 0 here, which reads back as
            // is_word_stripped=false — the same value the merge path hardcoded.
            buf.push(if m.is_word_stripped { 1 } else { 0 });
        }
        buf
    }
}

// ─── Reader ────────────────────────────────────────────────────────────────

/// Reads v3 term texts with metadata. Zero-copy over the source bytes.
pub struct TermTextsReaderV3<'a> {
    num_terms: u32,
    text_offsets: &'a [u8],
    text_data: &'a [u8],
    meta_data: Option<&'a [u8]>,
    meta_count: u32,
    max_word_content_len: Option<u16>,
}

impl<'a> TermTextsReaderV3<'a> {
    /// Open from raw file bytes.
    pub fn open(bytes: &'a [u8]) -> Option<Self> {
        let file = SectionFileReader::open(bytes, &MAGIC)?;

        // Read TEXTS section
        let texts_raw = file.get_section(SECTION_TEXTS)?;
        if texts_raw.len() < 4 {
            return None;
        }
        let num_terms = u32::from_le_bytes(texts_raw[0..4].try_into().ok()?);
        let offsets_size = (num_terms as usize + 1) * 4;
        if texts_raw.len() < 4 + offsets_size {
            return None;
        }
        let text_offsets = &texts_raw[4..4 + offsets_size];
        let text_data = &texts_raw[4 + offsets_size..];

        // Read META section (optional for forward compat)
        let (meta_data, meta_count) = if let Some(meta_raw) = file.get_section(SECTION_META) {
            if meta_raw.len() >= 4 {
                let count = u32::from_le_bytes(meta_raw[0..4].try_into().ok()?);
                (Some(&meta_raw[4..]), count)
            } else {
                (None, 0)
            }
        } else {
            (None, 0)
        };

        // Layout 1 (24 August): the word partition holds every word,
        // including words without a trailing separator. A 2-byte STATS
        // (23 August) was written by builders that skipped those words;
        // its max_word is unusable for skipping chunk chains → None.
        let max_word_content_len = file.get_section(SECTION_STATS)
            .filter(|s| s.len() >= 4 && u16::from_le_bytes([s[2], s[3]]) == STATS_VERSION)
            .map(|s| u16::from_le_bytes([s[0], s[1]]));

        Some(Self {
            num_terms,
            text_offsets,
            text_data,
            meta_data,
            meta_count,
            max_word_content_len,
        })
    }

    /// Get the extended token text for an ordinal.
    pub fn text(&self, ordinal: u32) -> Option<&'a str> {
        if ordinal >= self.num_terms {
            return None;
        }
        let start = self.read_text_offset(ordinal) as usize;
        let end = self.read_text_offset(ordinal + 1) as usize;
        if end > self.text_data.len() {
            return None;
        }
        std::str::from_utf8(&self.text_data[start..end]).ok()
    }

    /// Get the metadata for an ordinal.
    pub fn meta(&self, ordinal: u32) -> Option<TermMetaV3> {
        if ordinal >= self.meta_count {
            return None;
        }
        let data = self.meta_data?;
        let pos = ordinal as usize * 6;
        if pos + 6 > data.len() {
            return None;
        }
        Some(TermMetaV3 {
            own_len: u16::from_le_bytes([data[pos], data[pos + 1]]),
            sep_len: data[pos + 2],
            overlap_len: data[pos + 3],
            is_word_start: data[pos + 4] != 0,
            is_word_stripped: data[pos + 5] != 0,
        })
    }

    /// True when the token's own bytes (content + trailing separators, no
    /// overlap) hold at least one content byte — i.e. `own_len > sep_len`.
    ///
    /// This is the one question the relaxed-adjacency walkers ask of a chunk,
    /// and the `.bytemap` sidecar used to answer it with a 256-bit bitmap per
    /// ordinal (11 % of the index on the 93 605-file kernel corpus). Content
    /// bytes are exactly the bytes `is_content_char` accepts, so the two
    /// answers coincide (`bytemap_and_meta_agree_on_content`).
    ///
    /// Three bytes read at `6 × ordinal` in the META section. An ordinal past
    /// the META section (a file written without it) counts as content: the
    /// walker then refuses to step over it, which loses a match rather than
    /// inventing one.
    #[inline]
    pub fn has_content(&self, ordinal: u32) -> bool {
        if ordinal >= self.meta_count {
            return true;
        }
        let Some(data) = self.meta_data else { return true };
        let pos = ordinal as usize * 6;
        if pos + 3 > data.len() {
            return true;
        }
        let own_len = u16::from_le_bytes([data[pos], data[pos + 1]]);
        own_len > data[pos + 2] as u16
    }

    /// Get text + metadata together for an ordinal.
    pub fn entry(&self, ordinal: u32) -> Option<(&'a str, TermMetaV3)> {
        Some((self.text(ordinal)?, self.meta(ordinal)?))
    }

    /// Longest word-stripped content in this segment, if the file records it.
    /// `None` on files written before the STATS section.
    pub fn max_word_content_len(&self) -> Option<u16> {
        self.max_word_content_len
    }

    /// True unless the file proves that every word fits under
    /// [`WORD_SUFFIX_CAP`] — i.e. that the word pipeline alone reaches every
    /// in-word occurrence. Unknown is treated as "maybe".
    pub fn may_have_long_words(&self) -> bool {
        self.max_word_content_len.is_none_or(|m| m > WORD_SUFFIX_CAP)
    }

    /// Number of terms.
    pub fn num_terms(&self) -> u32 {
        self.num_terms
    }

    /// Iterate all entries: (ordinal, text, meta).
    pub fn iter(&self) -> impl Iterator<Item = (u32, &'a str, TermMetaV3)> + '_ {
        (0..self.num_terms).filter_map(move |ord| {
            let text = self.text(ord)?;
            let meta = self.meta(ord)?;
            Some((ord, text, meta))
        })
    }

    fn read_text_offset(&self, idx: u32) -> u32 {
        let pos = idx as usize * 4;
        u32::from_le_bytes(self.text_offsets[pos..pos + 4].try_into().unwrap())
    }
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_roundtrip_basic() {
        let mut writer = TermTextsWriterV3::new();
        writer.add(0, "mutex_lo", TermMetaV3 {
            own_len: 6, sep_len: 1, overlap_len: 2, is_word_start: true, is_word_stripped: true });
        writer.add(1, "lock", TermMetaV3 {
            own_len: 4, sep_len: 0, overlap_len: 0, is_word_start: true, is_word_stripped: false });

        let bytes = writer.serialize();
        let reader = TermTextsReaderV3::open(&bytes).unwrap();

        assert_eq!(reader.num_terms(), 2);
        assert_eq!(reader.text(0), Some("mutex_lo"));
        assert_eq!(reader.text(1), Some("lock"));

        let meta0 = reader.meta(0).unwrap();
        assert_eq!(meta0.own_len, 6);
        assert_eq!(meta0.sep_len, 1);
        assert_eq!(meta0.overlap_len, 2);
        assert!(meta0.is_word_start);

        let meta1 = reader.meta(1).unwrap();
        assert_eq!(meta1.own_len, 4);
        assert_eq!(meta1.sep_len, 0);
        assert_eq!(meta1.overlap_len, 0);
        assert!(meta1.is_word_start);

        // The partition tag must survive the round-trip: it is the only thing that
        // tells a reader whether an ordinal's postings live in .sfxpost (chunk) or
        // .word_sfxpost (word-stripped). Losing it is what lets a text-keyed
        // structure fuse the two halves of the model.
        assert!(meta0.is_word_stripped, "partition tag lost for ordinal 0");
        assert!(!meta1.is_word_stripped, "partition tag wrong for ordinal 1");
    }

    #[test]
    fn test_roundtrip_many() {
        let mut writer = TermTextsWriterV3::new();
        for i in 0..100 {
            let text = format!("token_{i}");
            writer.add(i, &text, TermMetaV3 {
                own_len: text.len() as u16,
                sep_len: 0,
                overlap_len: if i < 99 { 2 } else { 0 },
                is_word_start: i % 3 == 0,
                is_word_stripped: i % 7 == 0,
            });
        }

        let bytes = writer.serialize();
        let reader = TermTextsReaderV3::open(&bytes).unwrap();

        assert_eq!(reader.num_terms(), 100);
        for i in 0..100u32 {
            assert_eq!(reader.meta(i).unwrap().is_word_stripped, i % 7 == 0,
                "partition tag mismatch at ordinal {i}");
        }
        for i in 0..100 {
            let expected = format!("token_{i}");
            assert_eq!(reader.text(i), Some(expected.as_str()));
            let meta = reader.meta(i).unwrap();
            assert_eq!(meta.is_word_start, i % 3 == 0);
            assert_eq!(meta.overlap_len, if i < 99 { 2 } else { 0 });
        }
    }

    #[test]
    fn test_entry() {
        let mut writer = TermTextsWriterV3::new();
        writer.add(0, "getEleme", TermMetaV3 {
            own_len: 8, sep_len: 0, overlap_len: 2, is_word_start: true, is_word_stripped: false });

        let bytes = writer.serialize();
        let reader = TermTextsReaderV3::open(&bytes).unwrap();

        let (text, meta) = reader.entry(0).unwrap();
        assert_eq!(text, "getEleme");
        assert!(meta.is_word_start);
        assert_eq!(meta.overlap_len, 2);
    }

    #[test]
    fn test_iter() {
        let mut writer = TermTextsWriterV3::new();
        writer.add(0, "aaa", TermMetaV3 { own_len: 3, sep_len: 0, overlap_len: 0, is_word_start: true, is_word_stripped: false });
        writer.add(1, "bbb", TermMetaV3 { own_len: 3, sep_len: 0, overlap_len: 0, is_word_start: false, is_word_stripped: false });

        let bytes = writer.serialize();
        let reader = TermTextsReaderV3::open(&bytes).unwrap();

        let entries: Vec<_> = reader.iter().collect();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].1, "aaa");
        assert!(entries[0].2.is_word_start);
        assert_eq!(entries[1].1, "bbb");
        assert!(!entries[1].2.is_word_start);
    }

    #[test]
    fn test_out_of_bounds() {
        let mut writer = TermTextsWriterV3::new();
        writer.add(0, "hello", TermMetaV3 { own_len: 5, sep_len: 0, overlap_len: 0, is_word_start: true, is_word_stripped: false });

        let bytes = writer.serialize();
        let reader = TermTextsReaderV3::open(&bytes).unwrap();

        assert_eq!(reader.text(1), None);
        assert_eq!(reader.meta(1), None);
        assert_eq!(reader.entry(1), None);
    }

    #[test]
    fn test_empty() {
        let writer = TermTextsWriterV3::new();
        let bytes = writer.serialize();
        let reader = TermTextsReaderV3::open(&bytes).unwrap();

        assert_eq!(reader.num_terms(), 0);
        assert_eq!(reader.text(0), None);
    }

    #[test]
    fn test_utf8_text() {
        let mut writer = TermTextsWriterV3::new();
        writer.add(0, "café_la", TermMetaV3 {
            own_len: 6, sep_len: 1, overlap_len: 2, is_word_start: true, is_word_stripped: false });

        let bytes = writer.serialize();
        let reader = TermTextsReaderV3::open(&bytes).unwrap();

        assert_eq!(reader.text(0), Some("café_la"));
    }

    #[test]
    fn test_collector_to_termtexts() {
        use crate::suffix_fst::collector_v3::SfxCollectorV3;

        let mut c = SfxCollectorV3::new();
        c.begin_doc();
        c.add_value("mutex_lock_init");
        c.end_doc();

        let data = c.into_data();

        // Write termtexts v3 from collector data
        let mut writer = TermTextsWriterV3::new();
        for &intern_ord in &data.sorted_indices {
            let text = &data.token_texts[intern_ord as usize];
            let meta = &data.token_meta[intern_ord as usize];
            let content_ord = data.intern_to_final[intern_ord as usize];
            writer.add(content_ord, text, TermMetaV3 {
                own_len: meta.own_len,
                sep_len: meta.sep_len,
                overlap_len: meta.overlap_len,
                is_word_start: meta.is_word_start, is_word_stripped: false });
        }

        let bytes = writer.serialize();
        let reader = TermTextsReaderV3::open(&bytes).unwrap();

        // All tokens should be readable with metadata
        for ord in 0..reader.num_terms() {
            let (text, meta) = reader.entry(ord).unwrap();
            assert!(!text.is_empty());
            assert!(meta.own_len > 0 || meta.sep_len > 0);
        }
    }

    #[test]
    fn stats_section_records_longest_word() {
        let meta = |own: u16, ws: bool| TermMetaV3 {
            own_len: own, sep_len: 1, overlap_len: 0, is_word_start: true, is_word_stripped: ws,
        };
        let mut w = TermTextsWriterV3::new();
        w.add(0, "chunk", meta(300, false)); // chunk shapes never count
        w.add(1, "short", meta(6, true));
        let bytes = w.serialize();
        let r = TermTextsReaderV3::open(&bytes).unwrap();
        assert_eq!(r.max_word_content_len(), Some(5));
        assert!(!r.may_have_long_words());

        let mut w = TermTextsWriterV3::new();
        w.add(0, "long", meta(WORD_SUFFIX_CAP + 2, true)); // content = cap + 1
        let bytes = w.serialize();
        let r = TermTextsReaderV3::open(&bytes).unwrap();
        assert_eq!(r.max_word_content_len(), Some(WORD_SUFFIX_CAP + 1));
        assert!(r.may_have_long_words());

        // Empty file: no word at all, still provable.
        let bytes = TermTextsWriterV3::new().serialize();
        let r = TermTextsReaderV3::open(&bytes).unwrap();
        assert!(!r.may_have_long_words());
    }
}
