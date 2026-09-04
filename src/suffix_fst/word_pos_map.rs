//! Word-position map: for each (doc_id, position) → the word-stripped ordinal
//! whose word STARTS at that position, plus the word's length in chunks.
//!
//! This is to `.word_sfxpost` what `.posmap` is to `.sfxpost`: the exact inverse,
//! built from the same entries. It lets the word pipeline answer "which word
//! begins at position p of doc d?" with one lookup, instead of materialising
//! the posting list of every candidate ordinal and pairing it against the
//! active set — 57 million pair iterations on `uint64_t` relaxed over 50k files.
//!
//! The file used to hold a per-document word counter. Nothing read it at query
//! time; the merge read it only to write it back. Same shape, new content, new
//! magic so an old file is rejected rather than misread.
//!
//! Slot layout (u32), `WMP3`: `ordinal | span << 28`. The ordinal takes
//! 28 bits — the bound of a v3 segment since 4 September 2026 at night
//! (`SuffixFstBuilderV3::MAX_ORDINAL`); the `.sfx` record no longer caps it
//! at 24 — and `span = last_position - first_position` takes 4. A span of
//! 15 means "15 or more": the reader reports it as `SPAN_OVERFLOW` and the
//! caller falls back to the posting list for the true end. Positions where
//! no word starts hold u32::MAX.
//!
//! `WMP2` (24 bits of ordinal, 8 of span, overflow at 255) is still read.
//!
//! ```text
//! [4 bytes] magic "WMP3"
//! [4 bytes] num_docs: u32 LE
//! [8 bytes × (num_docs + 1)] offset table
//! Data section (per doc):
//!   [4 bytes × num_tokens] slots: u32 LE, one per position
//! ```

use super::index_registry::{SfxIndexFile, MergeStrategy};

/// Builds a word-position map during indexation.
pub struct WordPosMapWriter {
    docs: Vec<Vec<u32>>,
    /// An ordinal did not fit in the slot. The builder refuses such an
    /// ordinal first (`MAX_ORDINAL`), so this is a second guard: the map
    /// would lie by omission, so `serialize` emits nothing and readers fall
    /// back to the posting path.
    overflow: bool,
}

/// Ordinal bits in a `WMP3` slot; `SuffixFstBuilderV3::MAX_ORDINAL` is
/// `(1 << SLOT_ORDINAL_BITS) - 1`.
pub const SLOT_ORDINAL_BITS: u32 = 28;
const SLOT_ORDINAL_MASK: u32 = (1 << SLOT_ORDINAL_BITS) - 1;
/// Ordinal bits in a `WMP2` slot (read only).
const SLOT_ORDINAL_BITS_V2: u32 = 24;
const SLOT_ORDINAL_MASK_V2: u32 = (1 << SLOT_ORDINAL_BITS_V2) - 1;
/// Span value the reader reports for "the span did not fit, ask the posting
/// list": 15 or more in a `WMP3` slot, 255 or more in a `WMP2` one.
pub const SPAN_OVERFLOW: u32 = 255;
/// Largest span a `WMP3` slot holds; written for any span at or above it.
const SLOT_SPAN_MAX: u32 = (1 << (32 - SLOT_ORDINAL_BITS)) - 1;

impl Default for WordPosMapWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl WordPosMapWriter {
    /// Empty map with no documents.
    pub fn new() -> Self {
        Self { docs: Vec::new(), overflow: false }
    }

    /// Record that the word-stripped `ordinal` starts at `first` in `doc_id` and
    /// ends at `last` (inclusive, chunk positions).
    pub fn add_word(&mut self, doc_id: u32, first: u32, last: u32, ordinal: u32) {
        if ordinal > SLOT_ORDINAL_MASK {
            self.overflow = true;
            return;
        }
        let span = last.saturating_sub(first).min(SLOT_SPAN_MAX);
        let slot = ordinal | (span << SLOT_ORDINAL_BITS);

        let d = doc_id as usize;
        if d >= self.docs.len() {
            self.docs.resize(d + 1, Vec::new());
        }
        let p = first as usize;
        let doc = &mut self.docs[d];
        if p >= doc.len() {
            doc.resize(p + 1, u32::MAX);
        }
        // One word start per position is what makes this an exact inverse. The
        // collector guarantees it (a tail entry sits on the word's LAST chunk,
        // never its first); count rather than trust.
        if doc[p] != u32::MAX && doc[p] != slot {
            crate::suffix_fst::briques::profile::bump(|c| &c.n_wordmap_collisions, 1);
        }
        doc[p] = slot;
    }

    /// Raw slot write. Kept for the unit tests of the container format.
    pub fn add(&mut self, doc_id: u32, position: u32, slot: u32) {
        let d = doc_id as usize;
        if d >= self.docs.len() {
            self.docs.resize(d + 1, Vec::new());
        }
        let p = position as usize;
        let doc = &mut self.docs[d];
        if p >= doc.len() {
            doc.resize(p + 1, u32::MAX);
        }
        doc[p] = slot;
    }

    /// Serialize to binary format. Empty on ordinal overflow, so that
    /// `WordPosMapReader::open` fails and callers take the posting path.
    pub fn serialize(&self) -> Vec<u8> {
        if self.overflow {
            return Vec::new();
        }
        let num_docs = self.docs.len() as u32;
        let header_size = 4 + 4 + (num_docs as usize + 1) * 8;
        let data_size: usize = self.docs.iter().map(|d| d.len() * 4).sum();
        let mut buf = Vec::with_capacity(header_size + data_size);

        buf.extend_from_slice(b"WMP3");
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
    /// Ordinal bits of a slot: 28 (`WMP3`) or 24 (`WMP2`).
    ordinal_bits: u32,
}

impl<'a> WordPosMapReader<'a> {
    /// Open a `WMP3` or `WMP2` file over borrowed bytes; `None` on a
    /// different magic (including the older per-document counter format)
    /// or a truncated header.
    pub fn open(bytes: &'a [u8]) -> Option<Self> {
        if bytes.len() < 8 { return None; }
        let ordinal_bits = match &bytes[0..4] {
            b"WMP3" => SLOT_ORDINAL_BITS,
            b"WMP2" => SLOT_ORDINAL_BITS_V2,
            _ => return None,
        };
        let num_docs = u32::from_le_bytes(bytes[4..8].try_into().ok()?);
        let offsets_size = (num_docs as usize + 1) * 8;
        if bytes.len() < 8 + offsets_size { return None; }
        Some(Self {
            num_docs,
            offsets: &bytes[8..8 + offsets_size],
            data: &bytes[8 + offsets_size..],
            ordinal_bits,
        })
    }

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

    /// Raw slot at (doc_id, position): `ordinal | span << ordinal_bits`, or
    /// `None` when out of range or when no word starts there. See
    /// `word_start_at` for the decoded form.
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

    /// The word-stripped ordinal whose word starts at `position`, with its span
    /// in chunks. `span == SPAN_OVERFLOW` means the true span is at least that
    /// and must be read from the posting list.
    pub fn word_start_at(&self, doc_id: u32, position: u32) -> Option<(u32, u32)> {
        let slot = self.word_at(doc_id, position)?;
        if self.ordinal_bits == SLOT_ORDINAL_BITS_V2 {
            return Some((slot & SLOT_ORDINAL_MASK_V2, slot >> SLOT_ORDINAL_BITS_V2));
        }
        let span = slot >> SLOT_ORDINAL_BITS;
        Some((slot & SLOT_ORDINAL_MASK, if span >= SLOT_SPAN_MAX { SPAN_OVERFLOW } else { span }))
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
    /// Registry entry with an empty writer.
    pub fn new() -> Self {
        Self { writer: WordPosMapWriter::new() }
    }
}

impl SfxIndexFile for WordPosMapIndex {
    fn id(&self) -> &'static str { "word_pos_map" }
    fn extension(&self) -> &'static str { "word_pos_map" }
    /// v3 only: word-level partitioning does not exist in v2.
    fn written_for(&self, sfx_version: u8) -> bool { sfx_version >= 3 }
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
    fn test_word_start_at() {
        let mut w = WordPosMapWriter::new();
        // "internationalization" → one word of 3 chunks, ordinal 7, at positions 0..=2
        w.add_word(0, 0, 2, 7);
        // "mutex_lock" → two one-chunk words, ordinals 11 and 12
        w.add_word(0, 3, 3, 11);
        w.add_word(0, 4, 4, 12);

        let data = w.serialize();
        let r = WordPosMapReader::open(&data).unwrap();

        assert_eq!(r.word_start_at(0, 0), Some((7, 2)));
        assert_eq!(r.word_start_at(0, 1), None); // inside a word, no word starts here
        assert_eq!(r.word_start_at(0, 2), None);
        assert_eq!(r.word_start_at(0, 3), Some((11, 0)));
        assert_eq!(r.word_start_at(0, 4), Some((12, 0)));
        assert_eq!(r.word_start_at(0, 5), None);
        assert_eq!(r.word_start_at(1, 0), None);
    }

    #[test]
    fn test_span_overflow_and_ordinal_overflow() {
        let mut w = WordPosMapWriter::new();
        w.add_word(0, 0, 1000, 3);
        let r_data = w.serialize();
        let r = WordPosMapReader::open(&r_data).unwrap();
        assert_eq!(r.word_start_at(0, 0), Some((3, SPAN_OVERFLOW)));

        // A span of 15 already overflows a WMP3 slot; 14 does not.
        let mut w = WordPosMapWriter::new();
        w.add_word(0, 0, 15, 3);
        w.add_word(0, 16, 30, 4);
        let r_data = w.serialize();
        let r = WordPosMapReader::open(&r_data).unwrap();
        assert_eq!(r.word_start_at(0, 0), Some((3, SPAN_OVERFLOW)));
        assert_eq!(r.word_start_at(0, 16), Some((4, 14)));

        // An ordinal beyond 24 bits fits (28 bits); one beyond 28 disables the map.
        let mut w = WordPosMapWriter::new();
        w.add_word(0, 0, 2, 1 << 24);
        let r_data = w.serialize();
        assert_eq!(WordPosMapReader::open(&r_data).unwrap().word_start_at(0, 0), Some((1 << 24, 2)));
        let mut w = WordPosMapWriter::new();
        w.add_word(0, 0, 0, 1 << 28);
        assert!(w.serialize().is_empty());
        assert!(WordPosMapReader::open(&w.serialize()).is_none());
    }

    /// A `WMP2` file (24-bit ordinal, 8-bit span) still reads, with 255 as
    /// its overflow.
    #[test]
    fn wmp2_still_reads() {
        let mut buf = Vec::new();
        buf.extend_from_slice(b"WMP2");
        buf.extend_from_slice(&1u32.to_le_bytes());
        buf.extend_from_slice(&0u64.to_le_bytes());
        buf.extend_from_slice(&8u64.to_le_bytes());
        buf.extend_from_slice(&(7u32 | (2 << 24)).to_le_bytes());
        buf.extend_from_slice(&(9u32 | (255 << 24)).to_le_bytes());
        let r = WordPosMapReader::open(&buf).unwrap();
        assert_eq!(r.word_start_at(0, 0), Some((7, 2)));
        assert_eq!(r.word_start_at(0, 1), Some((9, SPAN_OVERFLOW)));
    }
}
