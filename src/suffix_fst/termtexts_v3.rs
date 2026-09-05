//! Term texts v3 — extended token texts + metadata for merge support.
//!
//! Uses the section_file format with magic "TTX3".
//!
//! Sections, layout 3 (container version 3, written since 4 September 2026
//! at night):
//!   0x05 — ENTRIES3: `[u32 num]`, a block-coded table of `num + 1` text
//!                offsets (`block_offsets`, 1 to 2 bytes each instead of 4),
//!                then `num` meta records of 4 bytes — `[u16 own_len]
//!                [u8 sep_len][u8 flags]` — then the concatenated UTF-8
//!                texts. The meta is read at `ordinal × 4`, one cache line
//!                for `meta()` / `has_content()`; `text()` pays the block
//!                table's two reads. 4 bytes + ~1.5 per ordinal against 8.
//!
//! Layout 2 (container version 2, 4 September 2026, still read):
//!   0x04 — ENTRIES: `[u32 num]`, then `num + 1` entries of 8 bytes —
//!                `[u32 text_offset][u16 own_len][u8 sep_len][u8 flags]` —
//!                then the concatenated UTF-8 texts. The meta sits next to
//!                the offset `text()` reads: `meta()` and `has_content()`
//!                touch the same cache line, where layout 1 paid a second
//!                random read into another section for each — measured as
//!                +3 ms on a relaxed fuzzy query over 30 000 files once the
//!                walkers read META instead of `.bytemap` and `gap_len`.
//!                8 bytes per ordinal instead of 4 + 6.
//!   0x03 — STATS: segment-wide facts derived from the meta at write time
//!                (max word-stripped content length). Optional: files
//!                written before it read back as "unknown", which every
//!                consumer must treat as the pessimistic answer.
//!
//! Layout 1 (container version 1), still read:
//!   0x01 — TEXTS: offset table + concatenated UTF-8 texts (same as v2 TTXT)
//!   0x02 — META:  per-ordinal 6-byte array (own_len, sep_len, overlap_len,
//!                is_word_start, is_word_stripped)
//!
//! The texts are the EXTENDED tokens (e.g., "mutex_lo" not "mutex_").
//! The metadata allows the merge process to re-feed tokens to the builder
//! without re-tokenizing.

use super::block_offsets::{self, BlockOffsets};
use super::section_file::{SectionFileReader, SectionFileWriter};

const MAGIC: [u8; 4] = *b"TTX3";
const VERSION: u8 = 3;

const SECTION_TEXTS: u16 = 0x01;
const SECTION_META: u16 = 0x02;
const SECTION_STATS: u16 = 0x03;
const SECTION_ENTRIES: u16 = 0x04;
const SECTION_ENTRIES3: u16 = 0x05;
/// The global ids the entries stand for, as runs, when the file is one
/// generation of a shard dictionary (or a segment's `.newtexts`): entry `i`
/// is id `run.start + (i - run.first_index)`. Absent = entry `i` is id `i`.
/// `[u32 runs]` then `[u32 start][u32 len]` per run, ascending.
const SECTION_IDS: u16 = 0x06;
/// Bytes per ordinal in the layout-3 meta table.
const META_SIZE: usize = 4;
/// Bytes per ordinal in the ENTRIES table.
const ENTRY_SIZE: usize = 8;
const OVERLAP_MASK: u8 = 0x0F;
const WORD_START_FLAG: u8 = 0x10;
const WORD_STRIPPED_FLAG: u8 = 0x20;
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
    /// The global id of each entry, ascending, when the entries are a
    /// subset of a shard's ids (`SECTION_IDS`).
    ids: Option<Vec<u32>>,
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
            ids: None,
        }
    }

    /// Entry `i` stands for global id `ids[i]` (ascending): a generation of
    /// a shard dictionary, or a segment's minted texts. `add` then takes the
    /// entry index, not the id.
    pub fn with_ids(mut self, ids: Vec<u32>) -> Self {
        debug_assert!(ids.windows(2).all(|w| w[0] < w[1]), "ids must be strictly increasing");
        self.ids = Some(ids);
        self
    }

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

    /// Add an extended token at the given ordinal with its metadata.
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

    /// Serialize to bytes using the section file format (layout 2: one
    /// ENTRIES section, see the module doc).
    pub fn serialize(&self) -> Vec<u8> {
        let mut file = SectionFileWriter::new(MAGIC, VERSION);

        file.add_section(SECTION_ENTRIES3, &self.serialize_entries());

        // Section STATS: max word-stripped content length (u16), then the
        // STATS layout version (u16). The version says what the word
        // partition is complete for: a reader that finds an older layout
        // must not trust `max_word` to skip the chunk chains.
        let max_word: u16 = self.metas.iter()
            .filter(|m| m.is_word_stripped)
            .map(|m| m.own_len.saturating_sub(m.sep_len as u16))
            .max()
            .unwrap_or(0);
        file.add_section(SECTION_STATS, &stats_section(max_word));

        if let Some(ids) = &self.ids {
            let mut runs: Vec<(u32, u32)> = Vec::new();
            for &id in ids {
                match runs.last_mut() {
                    Some((start, len)) if *start + *len == id => *len += 1,
                    _ => runs.push((id, 1)),
                }
            }
            file.add_section(SECTION_IDS, &ids_section(&runs));
        }

        file.serialize()
    }

    /// Layout 3 (module doc): `[u32 num]`, the block-coded text offsets
    /// (`num + 1`, the last being the texts' total length), `num` meta
    /// records of [`META_SIZE`] bytes — `[u16 own_len][u8 sep_len][u8
    /// flags]` — then the concatenated texts. `flags` = `overlap_len`
    /// (4 bits) | `is_word_start` (bit 4) | `is_word_stripped` (bit 5).
    fn serialize_entries(&self) -> Vec<u8> {
        let num = self.texts.len() as u32;
        let data_size: usize = self.texts.iter().map(|t| t.len()).sum();
        let mut offsets: Vec<u32> = Vec::with_capacity(num as usize + 1);
        let mut offset: u32 = 0;
        for text in &self.texts {
            offsets.push(offset);
            offset += text.len() as u32;
        }
        offsets.push(offset); // sentinel
        let table = block_offsets::encode(&offsets);

        let mut buf = Vec::with_capacity(4 + table.len() + num as usize * META_SIZE + data_size);
        buf.extend_from_slice(&num.to_le_bytes());
        buf.extend_from_slice(&table);
        for m in &self.metas {
            buf.extend_from_slice(&pack_meta(m));
        }
        for text in &self.texts {
            buf.extend_from_slice(text);
        }
        buf
    }
}

/// The layout-3 meta record of one entry: `[u16 own_len][u8 sep_len][u8 flags]`.
fn pack_meta(m: &TermMetaV3) -> [u8; META_SIZE] {
    assert!(m.overlap_len <= OVERLAP_MASK, "overlap_len {} does not fit in 4 bits", m.overlap_len);
    let [a, b] = m.own_len.to_le_bytes();
    [a, b, m.sep_len, m.overlap_len
        | if m.is_word_start { WORD_START_FLAG } else { 0 }
        | if m.is_word_stripped { WORD_STRIPPED_FLAG } else { 0 }]
}

/// The STATS section: the largest word-stripped content length, then the
/// STATS layout version.
fn stats_section(max_word: u16) -> [u8; 4] {
    let [a, b] = max_word.to_le_bytes();
    let [c, d] = STATS_VERSION.to_le_bytes();
    [a, b, c, d]
}

/// The IDS section over ids given ascending: `[u32 runs]` then
/// `[u32 start][u32 len]` per run.
fn ids_section(runs: &[(u32, u32)]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(4 + runs.len() * 8);
    buf.extend_from_slice(&(runs.len() as u32).to_le_bytes());
    for &(start, len) in runs {
        buf.extend_from_slice(&start.to_le_bytes());
        buf.extend_from_slice(&len.to_le_bytes());
    }
    buf
}

// ─── Streaming merge ────────────────────────────────────────────────────────

/// The entries of several files as one sequence ascending by id, an id
/// held by two files yielded once (the first file's). Each file's own
/// entries are ascending by id already (a generation of a shard dictionary
/// is written so); the merge is a heap over the files' cursors.
pub struct MergedEntries<'r, 'a> {
    cursors: Vec<std::iter::Peekable<Box<dyn Iterator<Item = (u32, &'a str, TermMetaV3)> + 'r>>>,
    heap: std::collections::BinaryHeap<std::cmp::Reverse<(u32, usize)>>,
    last: Option<u32>,
}

impl<'r, 'a: 'r> MergedEntries<'r, 'a> {
    /// Over `parts`, each read on its own ids.
    pub fn new(parts: &'r [TermTextsReaderV3<'a>]) -> Self {
        let mut cursors: Vec<std::iter::Peekable<Box<dyn Iterator<Item = (u32, &'a str, TermMetaV3)> + 'r>>> =
            parts.iter().map(|p| (Box::new(p.iter()) as Box<dyn Iterator<Item = _> + 'r>).peekable()).collect();
        let mut heap = std::collections::BinaryHeap::new();
        for (i, c) in cursors.iter_mut().enumerate() {
            if let Some(&(id, _, _)) = c.peek() {
                heap.push(std::cmp::Reverse((id, i)));
            }
        }
        Self { cursors, heap, last: None }
    }
}

impl<'r, 'a: 'r> Iterator for MergedEntries<'r, 'a> {
    type Item = (u32, &'a str, TermMetaV3);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let std::cmp::Reverse((id, i)) = self.heap.pop()?;
            let item = self.cursors[i].next()?;
            debug_assert_eq!(item.0, id);
            if let Some(&(next_id, _, _)) = self.cursors[i].peek() {
                debug_assert!(next_id > id, "a part's entries must ascend by id");
                self.heap.push(std::cmp::Reverse((next_id, i)));
            }
            if self.last == Some(id) {
                continue;
            }
            self.last = Some(id);
            return Some(item);
        }
    }
}

/// Write the union of `parts` to `out` as one layout-3 file with ids
/// (`SECTION_IDS`), ascending — the bytes `TermTextsWriterV3` would
/// produce over the same entries, without ever holding them: three passes
/// over the merge (offsets and ids, then the meta records, then the
/// texts), and the offset table is the only thing in RAM. Returns the
/// number of entries.
pub fn write_merged(parts: &[TermTextsReaderV3<'_>], out: &mut dyn std::io::Write) -> std::io::Result<u32> {
    use std::io::{Error, ErrorKind};
    let mut offsets: Vec<u32> = Vec::new();
    let mut runs: Vec<(u32, u32)> = Vec::new();
    let mut total: u64 = 0;
    let mut max_word: u16 = 0;
    for (id, text, m) in MergedEntries::new(parts) {
        offsets.push(total as u32);
        total += text.len() as u64;
        if total > u32::MAX as u64 {
            return Err(Error::new(ErrorKind::InvalidData, "term texts exceed 4 GB"));
        }
        if m.is_word_stripped {
            max_word = max_word.max(m.own_len.saturating_sub(m.sep_len as u16));
        }
        match runs.last_mut() {
            Some((start, len)) if *start + *len == id => *len += 1,
            _ => runs.push((id, 1)),
        }
    }
    let num = offsets.len() as u32;
    offsets.push(total as u32);
    let table = block_offsets::encode(&offsets);
    drop(offsets);
    let ids = ids_section(&runs);
    let entries_len = 4 + table.len() + num as usize * META_SIZE + total as usize;
    if entries_len > u32::MAX as usize {
        return Err(Error::new(ErrorKind::InvalidData, "term texts section exceeds 4 GB"));
    }
    let header = super::section_file::section_file_header(MAGIC, VERSION, &[
        (SECTION_ENTRIES3, entries_len as u32),
        (SECTION_STATS, 4),
        (SECTION_IDS, ids.len() as u32),
    ]);
    out.write_all(&header)?;
    out.write_all(&num.to_le_bytes())?;
    out.write_all(&table)?;
    for (_, _, m) in MergedEntries::new(parts) {
        out.write_all(&pack_meta(&m))?;
    }
    for (_, text, _) in MergedEntries::new(parts) {
        out.write_all(text.as_bytes())?;
    }
    out.write_all(&stats_section(max_word))?;
    out.write_all(&ids)?;
    Ok(num)
}

// ─── Reader ────────────────────────────────────────────────────────────────

/// Reads v3 term texts with metadata. Zero-copy over the source bytes.
///
/// Two layouts (module doc): layout 2 keeps offset and meta side by side in
/// `entries`, 8 bytes per ordinal; layout 1 keeps `entries` as a 4-byte offset
/// table and the meta in `legacy_meta`, 6 bytes per ordinal.
pub struct TermTextsReaderV3<'a> {
    num_terms: u32,
    /// `SECTION_IDS`: `(start, len, first entry index)` per run, ascending;
    /// an id is mapped to its entry before any read. `None` = id = index.
    id_runs: Option<Vec<(u32, u32, u32)>>,
    /// Further files answered for as one (the generations of a shard
    /// dictionary), each read on its own ids.
    more: Vec<TermTextsReaderV3<'a>>,
    /// Layout 3: the block-coded text offsets; `entries` is then the meta
    /// table alone, `stride` = [`META_SIZE`].
    text_offsets: Option<BlockOffsets<'a>>,
    /// `(num_terms + 1) × stride` bytes; the first 4 of each are the text offset.
    entries: &'a [u8],
    stride: usize,
    text_data: &'a [u8],
    /// Layout 1 only: the META section body (after its count), and that count.
    legacy_meta: Option<(&'a [u8], u32)>,
    max_word_content_len: Option<u16>,
}

impl<'a> TermTextsReaderV3<'a> {
    /// Open from raw file bytes.
    pub fn open(bytes: &'a [u8]) -> Option<Self> {
        let file = SectionFileReader::open(bytes, &MAGIC)?;

        let mut text_offsets = None;
        let (num_terms, entries, stride, text_data, legacy_meta) =
            if let Some(raw) = file.get_section(SECTION_ENTRIES3) {
                if raw.len() < 4 {
                    return None;
                }
                let num_terms = u32::from_le_bytes(raw[0..4].try_into().ok()?);
                let (table, used) = BlockOffsets::parse(&raw[4..])?;
                if table.len() != num_terms + 1 {
                    return None;
                }
                text_offsets = Some(table);
                let meta_start = 4 + used;
                let meta_end = meta_start + num_terms as usize * META_SIZE;
                if raw.len() < meta_end {
                    return None;
                }
                (num_terms, &raw[meta_start..meta_end], META_SIZE, &raw[meta_end..], None)
            } else if let Some(raw) = file.get_section(SECTION_ENTRIES) {
                if raw.len() < 4 {
                    return None;
                }
                let num_terms = u32::from_le_bytes(raw[0..4].try_into().ok()?);
                let table = (num_terms as usize + 1) * ENTRY_SIZE;
                if raw.len() < 4 + table {
                    return None;
                }
                (num_terms, &raw[4..4 + table], ENTRY_SIZE, &raw[4 + table..], None)
            } else {
                // Layout 1: TEXTS (offsets + texts) and META apart.
                let texts_raw = file.get_section(SECTION_TEXTS)?;
                if texts_raw.len() < 4 {
                    return None;
                }
                let num_terms = u32::from_le_bytes(texts_raw[0..4].try_into().ok()?);
                let offsets_size = (num_terms as usize + 1) * 4;
                if texts_raw.len() < 4 + offsets_size {
                    return None;
                }
                let legacy_meta = file.get_section(SECTION_META)
                    .filter(|m| m.len() >= 4)
                    .and_then(|m| Some((&m[4..], u32::from_le_bytes(m[0..4].try_into().ok()?))));
                (num_terms, &texts_raw[4..4 + offsets_size], 4, &texts_raw[4 + offsets_size..], legacy_meta)
            };

        // Layout 1 (24 August): the word partition holds every word,
        // including words without a trailing separator. A 2-byte STATS
        // (23 August) was written by builders that skipped those words;
        // its max_word is unusable for skipping chunk chains → None.
        let max_word_content_len = file.get_section(SECTION_STATS)
            .filter(|s| s.len() >= 4 && u16::from_le_bytes([s[2], s[3]]) == STATS_VERSION)
            .map(|s| u16::from_le_bytes([s[0], s[1]]));

        let id_runs = file.get_section(SECTION_IDS).and_then(|raw| {
            if raw.len() < 4 { return None; }
            let n = u32::from_le_bytes(raw[0..4].try_into().ok()?) as usize;
            if raw.len() < 4 + n * 8 { return None; }
            let mut runs = Vec::with_capacity(n);
            let mut first = 0u32;
            for i in 0..n {
                let o = 4 + i * 8;
                let start = u32::from_le_bytes(raw[o..o + 4].try_into().ok()?);
                let len = u32::from_le_bytes(raw[o + 4..o + 8].try_into().ok()?);
                runs.push((start, len, first));
                first += len;
            }
            Some(runs)
        });

        Some(Self {
            num_terms,
            id_runs,
            more: Vec::new(),
            text_offsets,
            entries,
            stride,
            text_data,
            legacy_meta,
            max_word_content_len,
        })
    }

    /// One reader over several files (the generations of a shard
    /// dictionary): an id is looked up in each in turn.
    pub fn open_parts(parts: &[&'a [u8]]) -> Option<Self> {
        let mut it = parts.iter();
        let mut first = Self::open(it.next()?)?;
        for bytes in it {
            first.more.push(Self::open(bytes)?);
        }
        Some(first)
    }

    /// True when entry indexes are not the ids (`SECTION_IDS`, or several files).
    pub fn has_id_map(&self) -> bool {
        self.id_runs.is_some() || !self.more.is_empty()
    }

    /// The entry index of a global id in THIS file, ignoring `more`.
    #[inline]
    fn local_index(&self, id: u32) -> Option<u32> {
        match &self.id_runs {
            None => (id < self.num_terms).then_some(id),
            Some(runs) => {
                let i = runs.partition_point(|&(start, _, _)| start <= id);
                if i == 0 { return None; }
                let (start, len, first) = runs[i - 1];
                (id < start + len).then(|| first + (id - start))
            }
        }
    }

    /// The file and entry index holding a global id, across `more`.
    #[inline]
    fn locate(&self, id: u32) -> Option<(&TermTextsReaderV3<'a>, u32)> {
        if let Some(i) = self.local_index(id) {
            return Some((self, i));
        }
        for part in &self.more {
            if let Some(i) = part.local_index(id) {
                return Some((part, i));
            }
        }
        None
    }

    /// The global id of entry `i` of this file (ignoring `more`).
    fn id_of_entry(&self, i: u32) -> u32 {
        match &self.id_runs {
            None => i,
            Some(runs) => {
                let k = runs.partition_point(|&(_, _, first)| first <= i);
                let (start, _, first) = runs[k.max(1) - 1];
                start + (i - first)
            }
        }
    }

    /// Layout 2 or 3 when true: the meta is one read per ordinal.
    pub fn has_inline_meta(&self) -> bool {
        self.stride != 4 || self.text_offsets.is_some()
    }

    /// The 4 meta bytes of an ordinal in layouts 2 and 3, `None` in layout 1.
    #[inline]
    fn inline_meta(&self, ordinal: u32) -> Option<&'a [u8]> {
        if ordinal >= self.num_terms {
            return None;
        }
        if self.text_offsets.is_some() {
            Some(&self.entries[ordinal as usize * META_SIZE..][..META_SIZE])
        } else if self.stride == ENTRY_SIZE {
            Some(&self.entries[ordinal as usize * ENTRY_SIZE + 4..][..META_SIZE])
        } else {
            None
        }
    }

    /// Get the extended token text for an ordinal (a global id when the
    /// file maps ids).
    pub fn text(&self, ordinal: u32) -> Option<&'a str> {
        let (part, i) = self.locate(ordinal)?;
        part.text_at(i)
    }

    /// Text of entry `i` of this file.
    fn text_at(&self, ordinal: u32) -> Option<&'a str> {
        if ordinal >= self.num_terms {
            return None;
        }
        let start = self.read_text_offset(ordinal) as usize;
        let end = self.read_text_offset(ordinal + 1) as usize;
        if end > self.text_data.len() || start > end {
            return None;
        }
        std::str::from_utf8(&self.text_data[start..end]).ok()
    }

    /// Get the metadata for an ordinal (a global id when the file maps ids).
    pub fn meta(&self, ordinal: u32) -> Option<TermMetaV3> {
        let (part, i) = self.locate(ordinal)?;
        part.meta_at(i)
    }

    /// Meta of entry `i` of this file.
    fn meta_at(&self, ordinal: u32) -> Option<TermMetaV3> {
        if self.has_inline_meta() {
            let e = self.inline_meta(ordinal)?;
            let flags = e[3];
            return Some(TermMetaV3 {
                own_len: u16::from_le_bytes([e[0], e[1]]),
                sep_len: e[2],
                overlap_len: flags & OVERLAP_MASK,
                is_word_start: flags & WORD_START_FLAG != 0,
                is_word_stripped: flags & WORD_STRIPPED_FLAG != 0,
            });
        }
        let (data, count) = self.legacy_meta?;
        if ordinal >= count {
            return None;
        }
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
    /// Layout 2: three bytes of the ordinal's 8-byte entry, the same line
    /// `text()` reads. An ordinal without meta (layout 1 file written without
    /// META) counts as content: the walker then refuses to step over it,
    /// which loses a match rather than inventing one.
    #[inline]
    pub fn has_content(&self, ordinal: u32) -> bool {
        let Some((part, i)) = self.locate(ordinal) else { return true };
        if part.has_inline_meta() {
            return match part.inline_meta(i) {
                Some(e) => u16::from_le_bytes([e[0], e[1]]) > e[2] as u16,
                None => true,
            };
        }
        part.meta_at(i).is_none_or(|m| m.own_len > m.sep_len as u16)
    }

    /// Get text + metadata together for an ordinal.
    pub fn entry(&self, ordinal: u32) -> Option<(&'a str, TermMetaV3)> {
        Some((self.text(ordinal)?, self.meta(ordinal)?))
    }

    /// Longest word-stripped content in this segment, if the file records it.
    /// `None` on files written before the STATS section.
    pub fn max_word_content_len(&self) -> Option<u16> {
        let mut m = self.max_word_content_len?;
        for part in &self.more {
            m = m.max(part.max_word_content_len?);
        }
        Some(m)
    }

    /// True unless the file proves that every word fits under
    /// [`WORD_SUFFIX_CAP`] — i.e. that the word pipeline alone reaches every
    /// in-word occurrence. Unknown is treated as "maybe".
    pub fn may_have_long_words(&self) -> bool {
        // Every file counted: a later dictionary generation may hold the
        // long word the first one does not.
        self.max_word_content_len().is_none_or(|m| m > WORD_SUFFIX_CAP)
    }

    /// Number of entries, every file counted.
    pub fn num_terms(&self) -> u32 {
        self.num_terms + self.more.iter().map(|p| p.num_terms).sum::<u32>()
    }

    /// Iterate all entries of every file: (global id, text, meta).
    /// The ids of every entry, across the parts, without reading a text —
    /// what a commit needs from a segment's `.newtexts` (decoding 950 000
    /// texts for their ids alone was 2 s of commit path on 30 000 files).
    pub fn ids(&self) -> impl Iterator<Item = u32> + '_ {
        std::iter::once(self).chain(self.more.iter()).flat_map(|part| match &part.id_runs {
            None => Box::new(0..part.num_terms) as Box<dyn Iterator<Item = u32> + '_>,
            Some(runs) => Box::new(runs.iter().flat_map(|&(start, len, _)| start..start + len)),
        })
    }

    /// Every entry across the parts: `(id, text, meta)`.
    pub fn iter(&self) -> impl Iterator<Item = (u32, &'a str, TermMetaV3)> + '_ {
        std::iter::once(self).chain(self.more.iter()).flat_map(|part| {
            (0..part.num_terms).filter_map(move |i| {
                let text = part.text_at(i)?;
                let meta = part.meta_at(i)?;
                Some((part.id_of_entry(i), text, meta))
            })
        })
    }

    #[inline]
    fn read_text_offset(&self, idx: u32) -> u32 {
        if let Some(t) = &self.text_offsets {
            return t.get(idx);
        }
        let pos = idx as usize * self.stride;
        u32::from_le_bytes(self.entries[pos..pos + 4].try_into().unwrap())
    }
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Layout 1 (TEXTS + META apart) as every file before 4 September 2026
    /// was written; the reader must give the same texts, meta and
    /// `has_content` as the layout-2 file of the same tokens.
    fn serialize_layout_1(w: &TermTextsWriterV3) -> Vec<u8> {
        let mut file = SectionFileWriter::new(MAGIC, 1);
        let num = w.texts.len() as u32;
        let mut texts = num.to_le_bytes().to_vec();
        let mut off = 0u32;
        for t in &w.texts { texts.extend_from_slice(&off.to_le_bytes()); off += t.len() as u32; }
        texts.extend_from_slice(&off.to_le_bytes());
        for t in &w.texts { texts.extend_from_slice(t); }
        file.add_section(SECTION_TEXTS, &texts);
        let mut meta = num.to_le_bytes().to_vec();
        for m in &w.metas {
            meta.extend_from_slice(&m.own_len.to_le_bytes());
            meta.push(m.sep_len); meta.push(m.overlap_len);
            meta.push(m.is_word_start as u8); meta.push(m.is_word_stripped as u8);
        }
        file.add_section(SECTION_META, &meta);
        file.serialize()
    }

    #[test]
    fn layout_1_and_layout_2_read_alike() {
        let mut w = TermTextsWriterV3::new();
        let metas = [
            TermMetaV3 { own_len: 6, sep_len: 1, overlap_len: 2, is_word_start: true, is_word_stripped: false },
            TermMetaV3 { own_len: 3, sep_len: 3, overlap_len: 0, is_word_start: false, is_word_stripped: false },
            TermMetaV3 { own_len: 300, sep_len: 0, overlap_len: 2, is_word_start: true, is_word_stripped: true },
            TermMetaV3 { own_len: 0, sep_len: 0, overlap_len: 0, is_word_start: false, is_word_stripped: false },
        ];
        for (i, (text, m)) in ["mutex_lo", "___", "internationalization", ""].iter().zip(metas).enumerate() {
            w.add(i as u32, text, m);
        }
        let d2 = w.serialize();
        let d1 = serialize_layout_1(&w);
        let (r2, r1) = (TermTextsReaderV3::open(&d2).unwrap(), TermTextsReaderV3::open(&d1).unwrap());
        assert!(r2.has_inline_meta() && !r1.has_inline_meta());
        assert_eq!(r2.num_terms(), 4);
        for ord in 0..5u32 {
            assert_eq!(r2.text(ord), r1.text(ord), "text {ord}");
            assert_eq!(r2.meta(ord), r1.meta(ord), "meta {ord}");
            assert_eq!(r2.has_content(ord), r1.has_content(ord), "has_content {ord}");
        }
        assert_eq!(r2.meta(2), Some(metas[2]));
        assert!(r2.has_content(0) && !r2.has_content(1) && !r2.has_content(3) && r2.has_content(4));
        assert_eq!(r2.iter().count(), r1.iter().count());
        // 8 bytes per ordinal against 4 + 6.
        assert!(d2.len() < d1.len());
    }

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
