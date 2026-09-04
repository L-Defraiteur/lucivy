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
//!       each (`encode_parent_entries_v3`), 4 September 2026
//!   6 — every key's parents are such a record, a single parent included, and
//!       the FST value is the record's offset — no inline parent, no flag bit.
//!       Offsets grow with the keys, which the FST shares along its paths, and
//!       the ordinal is a varint in the record: the 24-bit bound of the inline
//!       value no longer comes from this file. 4 September 2026, evening.
//!   7 — the key stops at the token boundary (no overlap bytes in it, hence no
//!       marker key either) and each parent's overlap bytes are in the record.
//!       Two chunks with the same own text and different overlaps share a
//!       key. An intermediate build of 4 September 2026, evening: refused.
//!   8 — same keys; the record is flat up to 32 parents and grouped by
//!       overlap beyond, and a parent no longer spells its `own_len` (the
//!       key implies it) nor a `sep_len` byte (three bits of the flags)
//!       (`encode_parent_entries_v8`). Written since 4 September 2026,
//!       night. `keys_cut_at_boundary()` tells the walk; decoding takes the key.
//!
//! Versions 3 to 5 keep an inline single parent in the FST value (bit 63 set
//! means "offset of a record" — `decode_output_v3`). The reader accepts
//! 3 to 6 and 8; the writer only emits 8.
//!
//! Removed vs v2: sibling table, gapmap, sepmap (all in separate files or gone).

use lucivy_fst::{Map, OutputTable};

use super::builder_v3::{
    decode_output_v3, decode_parent_entries_v3, decode_parent_entries_v3_legacy,
    decode_parent_entries_v4_packed, decode_parent_entries_v8, decode_parent_entries_v8_where,
    ParentEntryV3, ParentRefV3,
};
use super::section_file::{SectionFileReader, SectionFileWriter};

const MAGIC: [u8; 4] = *b"SFX3";
/// Container version written by `SfxFileWriterV3`.
pub const VERSION: u8 = 8;
/// Last container version whose FST value may hold a parent inline.
const INLINE_VALUE_VERSION: u8 = 5;
/// Last container version whose keys run into the overlap (and have markers).
const OVERLAP_IN_KEY_VERSION: u8 = 6;
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
    /// The container version is newer than this reader; carries the version byte.
    UnsupportedVersion(u8),
    /// Container version 7, written by an intermediate build of 4 September
    /// 2026 and never published: the index must be rebuilt.
    IntermediateVersion(u8),
}

impl std::fmt::Display for SfxV3Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SfxV3Error::InvalidFormat => write!(f, "invalid SFX3 format"),
            SfxV3Error::MissingSection(s) => write!(f, "missing section: {s}"),
            SfxV3Error::FstError(e) => write!(f, "FST error: {e}"),
            SfxV3Error::UnsupportedVersion(v) => write!(
                f, "SFX3 container version {v} is newer than this reader (up to {VERSION})"),
            SfxV3Error::IntermediateVersion(v) => write!(
                f, "SFX3 container version {v} was written by an intermediate build; rebuild the index"),
        }
    }
}

impl std::error::Error for SfxV3Error {}

/// Results of the FST-only briques (`fst_candidates_v3`, the falling walks,
/// the FST chains) keyed by their arguments, for a reader shared by every
/// segment of a shard: on a dictionary index (`sfx_version` 4) all the
/// segments walk the same FST for the same query, so the first walk
/// answers for all — 160 segments used to mean 160 walks of the whole
/// shard's dictionary. Bounded: the map is emptied past `MEMO_MAX_ENTRIES`.
///
/// Concurrent misses on one key compute once: the map hands out a cell
/// per key, and `OnceLock::get_or_init` makes the other callers wait.
pub struct FstMemo {
    entries: std::sync::Mutex<std::collections::HashMap<(u8, Vec<u8>, u8), std::sync::Arc<std::sync::OnceLock<std::sync::Arc<dyn std::any::Any + Send + Sync>>>>>,
}

const MEMO_MAX_ENTRIES: usize = 4096;

impl Default for FstMemo {
    fn default() -> Self { Self::new() }
}

impl FstMemo {
    /// An empty memo.
    pub fn new() -> Self {
        Self { entries: std::sync::Mutex::new(std::collections::HashMap::new()) }
    }

    /// True when `(tag, query, flags)` has a cell — computed or being computed.
    pub fn contains(&self, tag: u8, query: &[u8], flags: u8) -> bool {
        self.entries.lock().unwrap().contains_key(&(tag, query.to_vec(), flags))
    }

    /// The value under `(tag, query, flags)`, computed by `f` on the first
    /// call and shared afterwards.
    pub fn get_or_compute<T: std::any::Any + Send + Sync + 'static>(
        &self,
        tag: u8,
        query: &[u8],
        flags: u8,
        f: impl FnOnce() -> T,
    ) -> std::sync::Arc<T> {
        let cell = {
            let mut map = self.entries.lock().unwrap();
            if map.len() >= MEMO_MAX_ENTRIES {
                map.clear();
            }
            map.entry((tag, query.to_vec(), flags))
                .or_insert_with(|| std::sync::Arc::new(std::sync::OnceLock::new()))
                .clone()
        };
        let value = cell.get_or_init(|| std::sync::Arc::new(f()) as std::sync::Arc<dyn std::any::Any + Send + Sync>);
        value.clone().downcast::<T>().expect("memo entry of another type under this tag")
    }
}

/// Reads a .sfx v3 file.
#[derive(Clone)]
pub struct SfxFileReaderV3 {
    /// FST over an Arc-backed slice of the file: opening is O(1) and copies
    /// nothing. It used to be `Map<Vec<u8>>` built from `to_vec()`, and the
    /// prescan opens one reader per segment per query — 3.8 s of CPU per query
    /// on 800 segments, for a query with no results, before any search began.
    fst: Map<common::OwnedBytes>,
    parent_list_data: common::OwnedBytes,
    /// Container version: 3 to 6, or 8 (see the module doc).
    version: u8,
    /// Shared results of the FST briques (`FstMemo`); set on a reader that
    /// serves several segments.
    memo: Option<std::sync::Arc<FstMemo>>,
    /// On a dictionary index, the `.gmap` of the segment this reader is
    /// answering for: the shard-wide candidates and splits the memo holds
    /// are cut down to the ids this segment has before any posting is
    /// looked up (a segment of 64 documents holds 1 % of the shard's ids).
    segment_gmap: Option<common::OwnedBytes>,
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
        // A version byte below 3 predates the byte itself: those files are
        // version 3. A byte above ours is a file we cannot read — it used to
        // be clamped down, which would have served another layout's bytes.
        let version = file.version();
        if version > VERSION {
            return Err(SfxV3Error::UnsupportedVersion(version));
        }
        if version == 7 {
            return Err(SfxV3Error::IntermediateVersion(version));
        }
        let version = version.max(LEGACY_PARENTS_VERSION);

        Ok(Self { fst, parent_list_data, version, memo: None, segment_gmap: None })
    }

    /// Share the FST briques' results across this reader's users.
    pub fn with_memo(mut self, memo: std::sync::Arc<FstMemo>) -> Self {
        self.memo = Some(memo);
        self
    }

    /// The memo, when this reader is shared (see `FstMemo`).
    pub fn memo(&self) -> Option<&FstMemo> {
        self.memo.as_deref()
    }

    /// A view of this reader for one segment: the same FST and memo, the
    /// candidates and splits filtered to the ids in `gmap`.
    pub fn for_segment(&self, gmap: common::OwnedBytes) -> Self {
        Self {
            fst: self.fst.clone(),
            parent_list_data: self.parent_list_data.clone(),
            version: self.version,
            memo: self.memo.clone(),
            segment_gmap: Some(gmap),
        }
    }

    /// The segment's `.gmap` when this is a per-segment view.
    pub fn segment_gmap(&self) -> Option<super::gmap::GmapReader<'_>> {
        self.segment_gmap.as_ref().and_then(|b| super::gmap::GmapReader::open(b))
    }

    /// Container version this file was written with (3 to 6, or 8).
    pub fn container_version(&self) -> u8 {
        self.version
    }

    /// True when a key ends at its token's boundary and the parent record
    /// carries the overlap bytes (container version 8); false when the key
    /// runs `overlap_len` bytes into the next token and a marker key is cut
    /// at the boundary (versions 3 to 6). The walk and the range scan differ.
    #[inline]
    pub fn keys_cut_at_boundary(&self) -> bool {
        self.version > OVERLAP_IN_KEY_VERSION
    }

    /// Access the FST.
    pub fn fst(&self) -> &Map<common::OwnedBytes> {
        &self.fst
    }

    /// The parents behind a FST value whose overlap bytes satisfy `keep` —
    /// only the matching groups are read (container version 7). In an older
    /// file the overlap is in the key, not the record: every parent is
    /// returned and the caller's walk sorts them out.
    ///
    /// `key` is the FST key the value was found under, partition byte
    /// included: since version 8 a parent's `own_len` is derived from it.
    /// Older files ignore it.
    pub fn decode_parents_where(&self, value: u64, key: &[u8], keep: impl Fn(&[u8]) -> bool) -> Vec<ParentEntryV3> {
        if self.version > OVERLAP_IN_KEY_VERSION {
            let table = OutputTable::new(&self.parent_list_data);
            return decode_parent_entries_v8_where(table.get(value), key, keep);
        }
        self.decode_parents(value, key)
    }

    /// How many parents a FST value has, decoding none of them on a
    /// version-8 file (`count_parent_entries_v8`); older files decode.
    pub fn count_parents(&self, value: u64, key: &[u8]) -> usize {
        if self.version > OVERLAP_IN_KEY_VERSION {
            let table = OutputTable::new(&self.parent_list_data);
            return crate::suffix_fst::builder_v3::count_parent_entries_v8(table.get(value));
        }
        self.decode_parents(value, key).len()
    }

    /// Decode parent(s) from a FST output value found under `key` (see
    /// `decode_parents_where` for why the key).
    pub fn decode_parents(&self, value: u64, key: &[u8]) -> Vec<ParentEntryV3> {
        if self.version > INLINE_VALUE_VERSION {
            let table = OutputTable::new(&self.parent_list_data);
            return if self.version > OVERLAP_IN_KEY_VERSION {
                decode_parent_entries_v8(table.get(value), key)
            } else {
                decode_parent_entries_v3(table.get(value))
            };
        }
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
                results.extend(self.decode_parents(val, &key));
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
    use crate::suffix_fst::builder_v3::{SuffixFstBuilderV3, MAX_OVERLAP_BYTES};
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

        // "mutex_" should be findable at SI=0 (the key stops at the boundary)
        let parents = reader.resolve_suffix("mutex_");
        assert!(!parents.is_empty(), "should find mutex_");
        assert!(parents.iter().any(|p| p.sti == 0 && p.is_word_start && &p.overlap[..2] == b"lo"));

        // "x_" carries the cross-boundary trigram "x_l" through its overlap
        let parents = reader.resolve_suffix("x_");
        assert!(parents.iter().any(|p| p.sti == 4 && &p.overlap[..2] == b"lo"), "should find x_ + lo");
    }

    #[test]
    fn test_parent_metadata() {
        let bytes = build_sfx_v3(&["mutex_lock"]);
        let reader = SfxFileReaderV3::open(&bytes).unwrap();

        let parents = reader.resolve_suffix("mutex_");
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

        // "mutex_" from docs 0 and 1: one key, two parents, overlaps "lo" and "co"
        let overlaps: Vec<[u8; 2]> = reader.resolve_suffix("mutex_").iter()
            .map(|p| [p.overlap[0], p.overlap[1]]).collect();
        assert!(overlaps.contains(b"lo") && overlaps.contains(b"co"), "{overlaps:?}");
        // "hello_" from doc 2
        assert!(!reader.resolve_suffix("hello_").is_empty());
    }

    /// An index written before container version 4 must still open and
    /// decode its parents: same key, same list, from the 11-byte records.
    #[test]
    fn version_3_file_still_reads_its_parents() {
        use crate::suffix_fst::builder_v3::{
            encode_multi_parent_v3, encode_single_parent_v3, ParentEntryV3,
        };
        let a = ParentEntryV3 { raw_ordinal: 7, sti: 0, own_len: 6, sep_len: 1, overlap_len: 2, overlap: [0; MAX_OVERLAP_BYTES], is_word_start: true };
        let b = ParentEntryV3 { raw_ordinal: 9, sti: 2, own_len: 8, sep_len: 0, overlap_len: 2, overlap: [0; MAX_OVERLAP_BYTES], is_word_start: false };

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
        assert_eq!(reader4.resolve_suffix("lo"), vec![a.clone(), b.clone()]);

        // Version 5: delta-coded record behind the flag bit, single parent inline.
        let mut table5 = lucivy_fst::OutputTableBuilder::new();
        let offset5 = table5.add(&crate::suffix_fst::builder_v3::encode_parent_entries_v3(&[a.clone(), b.clone()]));
        let mut fst5 = lucivy_fst::MapBuilder::memory();
        fst5.insert(b"\x01lo", encode_multi_parent_v3(offset5)).unwrap();
        fst5.insert(b"\x01mu", encode_single_parent_v3(&a)).unwrap();
        let mut file5 = SectionFileWriter::new(MAGIC, INLINE_VALUE_VERSION);
        file5.add_section(SECTION_FST, &fst5.into_inner().unwrap());
        file5.add_section(SECTION_PARENTS, &table5.into_inner());
        let bytes5 = file5.serialize();
        let reader5 = SfxFileReaderV3::open(&bytes5).unwrap();
        assert_eq!(reader5.container_version(), 5);
        assert_eq!(reader5.resolve_suffix("lo"), vec![a.clone(), b.clone()]);
        assert_eq!(reader5.resolve_suffix("mu"), vec![a.clone()]);

        // Version 6: every key behind an offset, keys still carrying the overlap.
        let mut table6 = lucivy_fst::OutputTableBuilder::new();
        let off_lo = table6.add(&crate::suffix_fst::builder_v3::encode_parent_entries_v3(&[a.clone(), b.clone()]));
        let off_mu = table6.add(&crate::suffix_fst::builder_v3::encode_parent_entries_v3(&[a.clone()]));
        let mut fst6 = lucivy_fst::MapBuilder::memory();
        fst6.insert(b"\x01lo", off_lo).unwrap();
        fst6.insert(b"\x01mu", off_mu).unwrap();
        let mut file6 = SectionFileWriter::new(MAGIC, OVERLAP_IN_KEY_VERSION);
        file6.add_section(SECTION_FST, &fst6.into_inner().unwrap());
        file6.add_section(SECTION_PARENTS, &table6.into_inner());
        let reader6 = SfxFileReaderV3::open(&file6.serialize()).unwrap();
        assert_eq!(reader6.container_version(), 6);
        assert!(!reader6.keys_cut_at_boundary());
        assert_eq!(reader6.resolve_suffix("lo"), vec![a.clone(), b.clone()]);
        assert_eq!(reader6.resolve_suffix("mu"), vec![a.clone()]);

        // And a freshly written file says 8: keys cut at the boundary, the
        // overlap in the record. `mutex_` + `lo` is the key `mutex_` now.
        let fresh = SfxFileReaderV3::open(&build_sfx_v3(&["mutex_lock"])).unwrap();
        assert_eq!(fresh.container_version(), 8);
        assert!(fresh.keys_cut_at_boundary());
        assert!(fresh.resolve_suffix("mutex_lo").is_empty());
        let p = fresh.resolve_suffix("mutex_");
        assert_eq!(p.len(), 1);
        assert_eq!((p[0].overlap_len, &p[0].overlap[..2]), (2, &b"lo"[..]));

        // The intermediate version 7 is refused too.
        let mut file7 = SectionFileWriter::new(MAGIC, 7);
        file7.add_section(SECTION_FST, &[]);
        file7.add_section(SECTION_PARENTS, &[]);
        assert!(matches!(SfxFileReaderV3::open(&file7.serialize()), Err(SfxV3Error::IntermediateVersion(7))));

        // A version from the future is refused, not clamped.
        let mut file9 = SectionFileWriter::new(MAGIC, VERSION + 1);
        file9.add_section(SECTION_FST, &[]);
        file9.add_section(SECTION_PARENTS, &[]);
        assert!(matches!(SfxFileReaderV3::open(&file9.serialize()), Err(SfxV3Error::UnsupportedVersion(v)) if v == VERSION + 1));
    }

    /// Measurement, not a check: parent-list sizes by key length in a real
    /// `.sfx` (path in `SFX_FILE`). Tells what a lookup that stops at a token
    /// boundary would have to decode if the overlap left the keys.
    #[test]
    #[ignore]
    fn measure_parents_by_key_length() {
        use lucivy_fst::Streamer;
        let Ok(path) = std::env::var("SFX_FILE") else { return };
        let bytes = std::fs::read(&path).unwrap();
        let reader = SfxFileReaderV3::open(&bytes).unwrap();
        // (keys, parents, max parents, parents of the 10 largest) per (partition, key length)
        let mut stats: std::collections::BTreeMap<(u8, usize), (u64, u64, u64)> = Default::default();
        let mut stream = reader.fst().stream();
        while let Some((key, val)) = stream.next() {
            let n = reader.decode_parents(val, key).len() as u64;
            let e = stats.entry((key[0], (key.len() - 1).min(12))).or_insert((0, 0, 0));
            e.0 += 1; e.1 += n; e.2 = e.2.max(n);
        }
        eprintln!("partition  len   keys      parents   avg     max");
        for ((p, l), (k, n, mx)) in &stats {
            eprintln!("  0x{p:02x}     {l:>2}{} {k:>8} {n:>12} {:>7.1} {mx:>7}", if *l == 12 { "+" } else { " " }, *n as f64 / *k as f64);
        }
    }

    /// Measurement, not a check: what the `.sfx` of a real segment (`SFX_FILE`)
    /// would weigh under alternative encodings of the parents. Every layout
    /// keeps the same information; the point is to choose before rewriting
    /// the builder. Layouts:
    ///   base — the file as it is (inline single parent, table for the rest);
    ///   all-table — every key's parents in the table (v5 record), the FST
    ///               value is the record offset (monotone, so the FST shares it);
    ///   ord-sti — same, but a record is only (Δordinal, sti): the four other
    ///             fields are per-ordinal meta already stored in `.termtexts`;
    ///   no-marker — keys cut at the token boundary (marker and long key
    ///               merge), the record carries the overlap bytes per parent;
    ///   keys-only — FST with a zero value under every key: the cost of the keys.
    #[test]
    #[ignore]
    fn measure_sfx_layouts() {
        use lucivy_fst::{MapBuilder, OutputTableBuilder, Streamer};
        use crate::suffix_fst::builder_v3::encode_parent_entries_v3;
        use crate::suffix_fst::varint::write_varint;
        let Ok(path) = std::env::var("SFX_FILE") else { return };
        let bytes = std::fs::read(&path).unwrap();
        let reader = SfxFileReaderV3::open(&bytes).unwrap();
        let fst_len = reader.fst().as_fst().as_bytes().len();
        let table_len = reader.parent_list_data.len();

        // Gather everything once: (key, parents).
        let mut keys: Vec<(Vec<u8>, Vec<ParentEntryV3>)> = Vec::new();
        let mut stream = reader.fst().stream();
        while let Some((key, val)) = stream.next() {
            keys.push((key.to_vec(), reader.decode_parents(val, key)));
        }
        let n_keys = keys.len();
        let n_single = keys.iter().filter(|(_, p)| p.len() == 1).count();
        let n_parents: usize = keys.iter().map(|(_, p)| p.len()).sum();
        eprintln!("file {} : fst {} KB, parents {} KB | {} keys ({} single-parent), {} parents",
            path, fst_len / 1024, table_len / 1024, n_keys, n_single, n_parents);

        let fst_size = |items: &[(Vec<u8>, u64)]| -> usize {
            let mut b = MapBuilder::memory();
            for (k, v) in items { b.insert(k, *v).unwrap(); }
            b.into_inner().unwrap().len()
        };

        // keys-only
        let zero: Vec<(Vec<u8>, u64)> = keys.iter().map(|(k, _)| (k.clone(), 0u64)).collect();
        let keys_only = fst_size(&zero);
        eprintln!("keys-only : fst {} KB", keys_only / 1024);

        // all-table (v5 record for every key)
        let mut tb = OutputTableBuilder::new();
        let mut items: Vec<(Vec<u8>, u64)> = Vec::with_capacity(n_keys);
        for (k, p) in &keys {
            let off = tb.add(&encode_parent_entries_v3(p));
            items.push((k.clone(), off));
        }
        let t = tb.into_inner().len();
        let f = fst_size(&items);
        eprintln!("all-table : fst {} KB, table {} KB, total {} KB ({:+.1}%)",
            f / 1024, t / 1024, (f + t) / 1024,
            100.0 * ((f + t) as f64 - (fst_len + table_len) as f64) / (fst_len + table_len) as f64);

        // ord-sti (count, Δordinal varint, sti u8) — meta fetched from termtexts
        let mut tb = OutputTableBuilder::new();
        let mut items: Vec<(Vec<u8>, u64)> = Vec::with_capacity(n_keys);
        let mut max_sti = 0u16;
        for (k, p) in &keys {
            let mut sorted = p.clone();
            sorted.sort_by_key(|x| (x.raw_ordinal, x.sti));
            let mut rec = Vec::with_capacity(4 + sorted.len() * 4);
            write_varint(&mut rec, sorted.len() as u64);
            let mut prev = 0u64;
            for x in &sorted {
                write_varint(&mut rec, x.raw_ordinal - prev); prev = x.raw_ordinal;
                max_sti = max_sti.max(x.sti);
                rec.push(x.sti as u8);
            }
            let off = tb.add(&rec);
            items.push((k.clone(), off));
        }
        let t = tb.into_inner().len();
        let f = fst_size(&items);
        eprintln!("ord-sti   : fst {} KB, table {} KB, total {} KB ({:+.1}%) [max sti {}]",
            f / 1024, t / 1024, (f + t) / 1024,
            100.0 * ((f + t) as f64 - (fst_len + table_len) as f64) / (fst_len + table_len) as f64, max_sti);

        // no-marker: cut keys at the boundary, overlap bytes into the record
        let mut cut: std::collections::BTreeMap<Vec<u8>, Vec<(ParentEntryV3, Vec<u8>)>> = Default::default();
        for (k, p) in &keys {
            for x in p {
                let boundary = if k[0] == 0x02 {
                    (x.own_len as usize).saturating_sub(x.sep_len as usize).saturating_sub(x.sti as usize)
                } else {
                    (x.own_len as usize).saturating_sub(x.sti as usize)
                };
                let body = &k[1..];
                let (head, tail) = if body.len() > boundary { body.split_at(boundary) } else { (body, &[][..]) };
                let mut key = Vec::with_capacity(1 + head.len());
                key.push(k[0]); key.extend_from_slice(head);
                let list = cut.entry(key).or_default();
                if let Some(e) = list.iter_mut().find(|(y, _)| y.raw_ordinal == x.raw_ordinal && y.sti == x.sti) {
                    if tail.len() > e.1.len() { e.1 = tail.to_vec(); }
                } else {
                    list.push((x.clone(), tail.to_vec()));
                }
            }
        }
        let mut tb = OutputTableBuilder::new();
        let mut items: Vec<(Vec<u8>, u64)> = Vec::with_capacity(cut.len());
        let mut n_parents_cut = 0usize;
        for (k, list) in &cut {
            let parents: Vec<ParentEntryV3> = list.iter().map(|(p, _)| p.clone()).collect();
            let mut rec = encode_parent_entries_v3(&parents);
            // overlap bytes, in the record's parent order
            let mut sorted: Vec<&(ParentEntryV3, Vec<u8>)> = list.iter().collect();
            sorted.sort_by_key(|(p, _)| (p.raw_ordinal, p.sti));
            for (_, ov) in sorted { rec.extend_from_slice(ov); }
            n_parents_cut += parents.len();
            let off = tb.add(&rec);
            items.push((k.clone(), off));
        }
        let t = tb.into_inner().len();
        let f = fst_size(&items);
        eprintln!("no-marker : fst {} KB, table {} KB, total {} KB ({:+.1}%) | {} keys, {} parents",
            f / 1024, t / 1024, (f + t) / 1024,
            100.0 * ((f + t) as f64 - (fst_len + table_len) as f64) / (fst_len + table_len) as f64,
            cut.len(), n_parents_cut);

        // The production version-7 record (flat / grouped by overlap) over the
        // cut keys, with the file's ordinals, and with ordinals renumbered by
        // (word-stripped, overlap, text) as the collector does since the
        // grouped record exists (needs `TERMTEXTS_FILE`, the segment's).
        {
            use crate::suffix_fst::builder_v3::encode_parent_entries_v8;
            let remap: Option<Vec<u64>> = std::env::var("TERMTEXTS_FILE").ok().map(|tp| {
                let tb = std::fs::read(&tp).unwrap();
                let tt = crate::suffix_fst::termtexts_v3::TermTextsReaderV3::open(&tb).unwrap();
                let n = tt.num_terms();
                let mut keyed: Vec<(bool, Vec<u8>, String, u32)> = (0..n).map(|o| {
                    let (text, m) = tt.entry(o).unwrap();
                    let lower = text.to_lowercase();
                    let ov = lower.as_bytes()[lower.len().saturating_sub(m.overlap_len as usize)..].to_vec();
                    (m.is_word_stripped, ov, lower, o)
                }).collect();
                keyed.sort();
                let mut map = vec![0u64; n as usize];
                for (new, (_, _, _, old)) in keyed.iter().enumerate() { map[*old as usize] = new as u64; }
                map
            });
            for (label, map) in [("v8 production", None), ("v8 + ordinals by overlap", remap.as_ref())] {
                if label.starts_with("v8 +") && map.is_none() { continue; }
                let mut tb = OutputTableBuilder::new();
                let mut items: Vec<(Vec<u8>, u64)> = Vec::with_capacity(cut.len());
                let mut flat = 0usize; let mut grouped = 0usize;
                for (k, list) in &cut {
                    let parents: Vec<ParentEntryV3> = list.iter().map(|(p, ov)| {
                        let mut q = p.clone();
                        q.overlap = [0; 4];
                        q.overlap[..ov.len().min(4)].copy_from_slice(&ov[..ov.len().min(4)]);
                        q.overlap_len = ov.len().min(4) as u8;
                        if let Some(m) = map { q.raw_ordinal = m[q.raw_ordinal as usize]; }
                        q
                    }).collect();
                    let rec = encode_parent_entries_v8(&parents, k);
                    if rec[0] & 0x80 != 0 { grouped += 1 } else { flat += 1 }
                    let off = tb.add(&rec);
                    items.push((k.clone(), off));
                }
                let t = tb.into_inner().len();
                let f = fst_size(&items);
                eprintln!("{label:>26}: fst {} KB, table {} KB, total {} KB ({:+.1}%) | {} flat, {} grouped",
                    f / 1024, t / 1024, (f + t) / 1024,
                    100.0 * ((f + t) as f64 - (fst_len + table_len) as f64) / (fst_len + table_len) as f64, flat, grouped);
            }
        }

        // no-marker + ord-sti + overlap bytes
        let mut tb = OutputTableBuilder::new();
        let mut items: Vec<(Vec<u8>, u64)> = Vec::with_capacity(cut.len());
        for (k, list) in &cut {
            let mut sorted: Vec<&(ParentEntryV3, Vec<u8>)> = list.iter().collect();
            sorted.sort_by_key(|(p, _)| (p.raw_ordinal, p.sti));
            let mut rec = Vec::new();
            write_varint(&mut rec, sorted.len() as u64);
            let mut prev = 0u64;
            for (p, ov) in &sorted {
                write_varint(&mut rec, p.raw_ordinal - prev); prev = p.raw_ordinal;
                rec.push(p.sti as u8);
                rec.push(ov.len() as u8);
                rec.extend_from_slice(ov);
            }
            let off = tb.add(&rec);
            items.push((k.clone(), off));
        }
        let t = tb.into_inner().len();
        let f = fst_size(&items);
        eprintln!("no-marker+ord-sti : fst {} KB, table {} KB, total {} KB ({:+.1}%)",
            f / 1024, t / 1024, (f + t) / 1024,
            100.0 * ((f + t) as f64 - (fst_len + table_len) as f64) / (fst_len + table_len) as f64);
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
