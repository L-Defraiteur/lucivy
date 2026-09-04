//! SFX file format v3 — section-based, no sibling table, no gapmap.
//!
//! Uses the section_file container with magic "SFX3".
//!
//! Sections:
//!   0x01 — FST: suffix FST bytes
//!   0x02 — PARENTS: OutputTable bytes (multi-parent records)
//!
//! Container versions (the byte after the magic; the magic stays `SFX3`, so
//! `detect_sfx_version` keeps answering 3 — the engine did not change):
//!   3 — a record is `[u32 count]` + 11 bytes per parent
//!   4 — a record is `[varint count]` + the packed 8-byte parent value
//!       (`decode_parent_entries_v4_packed`), 4 September 2026, morning
//!   5 — a record is `[varint count]` + delta-coded parents, about 5 bytes
//!       each (`encode_parent_entries_v3`), written since 4 September 2026
//!
//! The reader accepts all three; the writer only emits 5.
//!
//! Removed vs v2: sibling table, gapmap, sepmap (all in separate files or gone).

use lucivy_fst::{Map, OutputTable};

use super::builder_v3::{
    decode_output_v3, decode_parent_entries_v3, decode_parent_entries_v3_legacy,
    decode_parent_entries_v4_packed, ParentEntryV3, ParentRefV3,
};
use super::section_file::{SectionFileReader, SectionFileWriter};

const MAGIC: [u8; 4] = *b"SFX3";
/// Container version written by `SfxFileWriterV3`.
pub const VERSION: u8 = 5;
/// Last container version whose parent records use the 11-byte layout.
const LEGACY_PARENTS_VERSION: u8 = 3;
/// The container version whose records are packed 8-byte values.
const PACKED_PARENTS_VERSION: u8 = 4;

/// Section IDs for the .sfx v3 file.
pub const SECTION_FST: u16 = 0x01;
/// Section holding the OutputTable bytes (multi-parent records, v3 encoding).
pub const SECTION_PARENTS: u16 = 0x02;
// ─── Writer ────────────────────────────────────────────────────────────────

/// Assembles a .sfx v3 file from pre-built components.
pub struct SfxFileWriterV3 {
    fst_data: Vec<u8>,
    parent_list_data: Vec<u8>,
}

impl SfxFileWriterV3 {
    /// Writer over already-built FST bytes and OutputTable (parent list) bytes.
    pub fn new(fst_data: Vec<u8>, parent_list_data: Vec<u8>) -> Self {
        Self { fst_data, parent_list_data }
    }

    /// Serialize to bytes using the section file format.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut file = SectionFileWriter::new(MAGIC, VERSION);
        file.add_section(SECTION_FST, &self.fst_data);
        file.add_section(SECTION_PARENTS, &self.parent_list_data);
        file.serialize()
    }
}

// ─── Reader ────────────────────────────────────────────────────────────────

/// Error type for SFX v3 file operations.
#[derive(Debug)]
pub enum SfxV3Error {
    /// The bytes are not a valid `SFX3` section file (bad magic or truncated header).
    InvalidFormat,
    /// A required section is absent; carries the section's name.
    MissingSection(&'static str),
    /// The FST section could not be opened; carries the underlying error message.
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
    /// FST over an Arc-backed slice of the file: opening is O(1) and copies
    /// nothing. It used to be `Map<Vec<u8>>` built from `to_vec()`, and the
    /// prescan opens one reader per segment per query — 3.8 s of CPU per query
    /// on 800 segments, for a query with no results, before any search began.
    fst: Map<common::OwnedBytes>,
    parent_list_data: common::OwnedBytes,
    /// Container version: 3, 4 or 5 (see the module doc).
    version: u8,
}

impl SfxFileReaderV3 {
    /// Open from raw bytes.
    /// Open from a borrowed slice. Copies the data once into owned bytes;
    /// prefer `open_owned` on the query path, where the bytes already live in
    /// an `OwnedBytes` (mmap or RAM directory) and can be sliced for free.
    pub fn open(data: &[u8]) -> Result<Self, SfxV3Error> {
        Self::open_owned(common::OwnedBytes::new(data.to_vec()))
    }

    /// Open over Arc-backed bytes without copying any section.
    pub fn open_owned(data: common::OwnedBytes) -> Result<Self, SfxV3Error> {
        let file = SectionFileReader::open(&data, &MAGIC)
            .ok_or(SfxV3Error::InvalidFormat)?;
        let base = data.as_slice().as_ptr() as usize;
        let sub = |sec: &[u8]| -> common::OwnedBytes {
            let off = sec.as_ptr() as usize - base;
            data.slice(off..off + sec.len())
        };

        let fst_bytes = file.get_section(SECTION_FST)
            .ok_or(SfxV3Error::MissingSection("FST"))?;
        let fst = if fst_bytes.is_empty() {
            let empty = lucivy_fst::MapBuilder::memory().into_inner().unwrap_or_default();
            Map::new(common::OwnedBytes::new(empty))
                .map_err(|e| SfxV3Error::FstError(e.to_string()))?
        } else {
            Map::new(sub(fst_bytes))
                .map_err(|e| SfxV3Error::FstError(e.to_string()))?
        };

        let parent_list_data = sub(file.get_section(SECTION_PARENTS)
            .ok_or(SfxV3Error::MissingSection("PARENTS"))?);
        let version = file.version().clamp(LEGACY_PARENTS_VERSION, VERSION);

        Ok(Self { fst, parent_list_data, version })
    }

    /// Container version this file was written with (3, 4 or 5).
    pub fn container_version(&self) -> u8 {
        self.version
    }

    /// Access the FST.
    pub fn fst(&self) -> &Map<common::OwnedBytes> {
        &self.fst
    }

    /// Decode parent(s) from a FST output value.
    pub fn decode_parents(&self, value: u64) -> Vec<ParentEntryV3> {
        match decode_output_v3(value) {
            ParentRefV3::Single(entry) => vec![entry],
            ParentRefV3::Multi { offset } => {
                let table = OutputTable::new(&self.parent_list_data);
                let record = table.get(offset);
                match self.version {
                    v if v <= LEGACY_PARENTS_VERSION => decode_parent_entries_v3_legacy(record),
                    PACKED_PARENTS_VERSION => decode_parent_entries_v4_packed(record),
                    _ => decode_parent_entries_v3(record),
                }
            }
        }
    }

    /// Resolve all parents for a suffix string (for testing/debugging).
    pub fn resolve_suffix(&self, suffix: &str) -> Vec<ParentEntryV3> {
        let lower = suffix.to_lowercase();
        let mut results = Vec::new();

        for &prefix in &[super::builder::SI0_PREFIX, super::builder::SI_REST_PREFIX, super::builder_v3::SI_STRIPPED_PREFIX] {
            let mut key = vec![prefix];
            key.extend_from_slice(lower.as_bytes());
            if let Some(val) = self.fst.get(&key) {
                results.extend(self.decode_parents(val));
            }
        }
        results
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
        for &intern_ord in &data.sorted_indices {
            let meta = &data.token_meta[intern_ord as usize];
            if meta.is_word_stripped { continue; }
            let text = &data.token_texts[intern_ord as usize];
            let content_ord = data.intern_to_final[intern_ord as usize];
            builder.add_token(
                text,
                content_ord as u64,
                meta.own_len,
                meta.sep_len,
                meta.overlap_len,
                meta.is_word_start,
            );
        }
        let (fst_bytes, output_table) = builder.build().unwrap();

        SfxFileWriterV3::new(fst_bytes, output_table).to_bytes()
    }

    #[test]
    fn test_write_read_roundtrip() {
        let bytes = build_sfx_v3(&["mutex_lock"]);
        let reader = SfxFileReaderV3::open(&bytes).unwrap();

        assert!(reader.num_suffix_terms() > 0);
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

    /// An index written before container version 4 must still open and
    /// decode its parents: same key, same list, from the 11-byte records.
    #[test]
    fn version_3_file_still_reads_its_parents() {
        use crate::suffix_fst::builder_v3::{
            encode_multi_parent_v3, encode_single_parent_v3, ParentEntryV3,
        };
        let a = ParentEntryV3 { raw_ordinal: 7, sti: 0, own_len: 6, sep_len: 1, overlap_len: 2, is_word_start: true };
        let b = ParentEntryV3 { raw_ordinal: 9, sti: 2, own_len: 8, sep_len: 0, overlap_len: 2, is_word_start: false };

        // Legacy record for key "lo": [u32 count] + 11 bytes per parent.
        let mut record = 2u32.to_le_bytes().to_vec();
        for p in [&a, &b] {
            record.extend_from_slice(&(p.raw_ordinal as u32).to_le_bytes());
            record.extend_from_slice(&p.sti.to_le_bytes());
            record.extend_from_slice(&p.own_len.to_le_bytes());
            record.push(p.sep_len);
            record.push(p.overlap_len);
            record.push(if p.is_word_start { 1 } else { 0 });
        }
        let mut table = lucivy_fst::OutputTableBuilder::new();
        let offset = table.add(&record);

        let mut fst = lucivy_fst::MapBuilder::memory();
        fst.insert(b"\x01lo", encode_multi_parent_v3(offset)).unwrap();
        fst.insert(b"\x01mu", encode_single_parent_v3(&a)).unwrap();

        let mut file = SectionFileWriter::new(MAGIC, LEGACY_PARENTS_VERSION);
        file.add_section(SECTION_FST, &fst.into_inner().unwrap());
        file.add_section(SECTION_PARENTS, &table.into_inner());
        let bytes = file.serialize();

        let reader = SfxFileReaderV3::open(&bytes).unwrap();
        assert_eq!(reader.container_version(), 3);
        assert_eq!(reader.resolve_suffix("lo"), vec![a.clone(), b.clone()]);
        assert_eq!(reader.resolve_suffix("mu"), vec![a.clone()]);

        // Version 4: varint count + packed u64 per parent.
        let mut record4 = vec![2u8];
        for p in [&a, &b] { record4.extend_from_slice(&encode_single_parent_v3(p).to_le_bytes()); }
        let mut table4 = lucivy_fst::OutputTableBuilder::new();
        let offset4 = table4.add(&record4);
        let mut fst4 = lucivy_fst::MapBuilder::memory();
        fst4.insert(b"\x01lo", encode_multi_parent_v3(offset4)).unwrap();
        let mut file4 = SectionFileWriter::new(MAGIC, PACKED_PARENTS_VERSION);
        file4.add_section(SECTION_FST, &fst4.into_inner().unwrap());
        file4.add_section(SECTION_PARENTS, &table4.into_inner());
        let bytes4 = file4.serialize();
        let reader4 = SfxFileReaderV3::open(&bytes4).unwrap();
        assert_eq!(reader4.container_version(), 4);
        assert_eq!(reader4.resolve_suffix("lo"), vec![a, b]);

        // And a freshly written file says 5.
        let fresh = SfxFileReaderV3::open(&build_sfx_v3(&["mutex_lock"])).unwrap();
        assert_eq!(fresh.container_version(), 5);
    }

    /// Measurement, not a check: parent-list sizes by key length in a real
    /// `.sfx` (path in `SFX_FILE`). Tells what a lookup that stops at a token
    /// boundary would have to decode if the overlap left the keys.
    #[test]
    #[ignore]
    fn measure_parents_by_key_length() {
        use lucivy_fst::{IntoStreamer, Streamer};
        let Ok(path) = std::env::var("SFX_FILE") else { return };
        let bytes = std::fs::read(&path).unwrap();
        let reader = SfxFileReaderV3::open(&bytes).unwrap();
        // (keys, parents, max parents, parents of the 10 largest) per (partition, key length)
        let mut stats: std::collections::BTreeMap<(u8, usize), (u64, u64, u64)> = Default::default();
        let mut stream = reader.fst().stream();
        while let Some((key, val)) = stream.next() {
            let n = reader.decode_parents(val).len() as u64;
            let e = stats.entry((key[0], (key.len() - 1).min(12))).or_insert((0, 0, 0));
            e.0 += 1; e.1 += n; e.2 = e.2.max(n);
        }
        eprintln!("partition  len   keys      parents   avg     max");
        for ((p, l), (k, n, mx)) in &stats {
            eprintln!("  0x{p:02x}     {l:>2}{} {k:>8} {n:>12} {:>7.1} {mx:>7}", if *l == 12 { "+" } else { " " }, *n as f64 / *k as f64);
        }
    }

    #[test]
    fn test_empty_sfx() {
        let writer = SfxFileWriterV3::new(
            lucivy_fst::MapBuilder::memory().into_inner().unwrap(),
            Vec::new(),
        );
        let bytes = writer.to_bytes();
        let reader = SfxFileReaderV3::open(&bytes).unwrap();
        assert_eq!(reader.num_suffix_terms(), 0);
    }
}
