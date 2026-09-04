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
//!       value no longer comes from this file. Written since 4 September 2026, evening.
//!
//! Versions 3 to 5 keep an inline single parent in the FST value (bit 63 set
//! means "offset of a record" — `decode_output_v3`). The reader accepts all
//! four versions; the writer only emits 6.
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
pub const VERSION: u8 = 6;
/// Last container version whose FST value may hold a parent inline.
const INLINE_VALUE_VERSION: u8 = 5;
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
}

impl std::fmt::Display for SfxV3Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SfxV3Error::InvalidFormat => write!(f, "invalid SFX3 format"),
            SfxV3Error::MissingSection(s) => write!(f, "missing section: {s}"),
            SfxV3Error::FstError(e) => write!(f, "FST error: {e}"),
            SfxV3Error::UnsupportedVersion(v) => write!(
                f, "SFX3 container version {v} is newer than this reader (up to {VERSION})"),
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
    /// Container version: 3 to 6 (see the module doc).
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
        // A version byte below 3 predates the byte itself: those files are
        // version 3. A byte above ours is a file we cannot read — it used to
        // be clamped down, which would have served another layout's bytes.
        let version = file.version();
        if version > VERSION {
            return Err(SfxV3Error::UnsupportedVersion(version));
        }
        let version = version.max(LEGACY_PARENTS_VERSION);

        Ok(Self { fst, parent_list_data, version })
    }

    /// Container version this file was written with (3 to 6).
    pub fn container_version(&self) -> u8 {
        self.version
    }

    /// Access the FST.
    pub fn fst(&self) -> &Map<common::OwnedBytes> {
        &self.fst
    }

    /// Decode parent(s) from a FST output value.
    pub fn decode_parents(&self, value: u64) -> Vec<ParentEntryV3> {
        if self.version > INLINE_VALUE_VERSION {
            let table = OutputTable::new(&self.parent_list_data);
            return decode_parent_entries_v3(table.get(value));
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

        // And a freshly written file says 6, with every parent behind an offset.
        let fresh = SfxFileReaderV3::open(&build_sfx_v3(&["mutex_lock"])).unwrap();
        assert_eq!(fresh.container_version(), 6);
        assert!(!fresh.resolve_suffix("mutex_lo").is_empty());

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
            let n = reader.decode_parents(val).len() as u64;
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
            keys.push((key.to_vec(), reader.decode_parents(val)));
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
