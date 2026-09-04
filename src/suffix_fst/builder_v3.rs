//! SFX Builder v3 — overlap-aware suffix FST construction.
//!
//! Changes vs v2:
//! - Output u64 encodes: is_word_start, overlap_len, sep_len, own_len, sti, ordinal
//! - `add_token` takes extended token bytes (content + sep + overlap)
//! - Overlap bytes from the next token are appended by the caller (collector)

use lucivy_fst::{MapBuilder, OutputTableBuilder};

use super::builder::{SI0_PREFIX, SI_REST_PREFIX};

/// Prefix byte for sep-stripped entries (content + overlap, sep removed).
/// Used by strict_separators=false queries to match trigrams across sep zones.
pub const SI_STRIPPED_PREFIX: u8 = 0x02;

/// Max suffix depth in bytes.
const MAX_CHUNK_BYTES: usize = 256;

fn default_min_suffix_len() -> usize {
    std::env::var("LUCIVY_MIN_SUFFIX_LEN")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1)
}

// ─── V3 encoding layout ───────────────────────────────────────────────────
//
// Container version 6 (written since 4 September 2026, evening): the FST value is the
// offset of the key's parent record in the OutputTable, for one parent as for
// many (`encode_parent_entries_v3`). The layout below is what versions 3 to 5
// packed into the value itself; the reader still decodes it, the builder no
// longer writes it.
//
// Single parent (bit 63 = 0):
//   [63]     multi_flag = 0
//   [62]     is_word_start
//   [61..58] overlap_len    (4 bits, 0..15)
//   [57..50] sep_len        (8 bits, 0..255)
//   [49..36] own_len        (14 bits, max 16383)
//   [35..24] sti            (12 bits, max 4095)
//   [23..0]  token_ordinal  (24 bits)
//
// Multi parent (bit 63 = 1):
//   [62..0]  offset into OutputTable

const MULTI_FLAG: u64 = 1 << 63;
const WORD_START_FLAG: u64 = 1 << 62;

const ORDINAL_BITS: u32 = 24;
const ORDINAL_MASK: u64 = (1 << ORDINAL_BITS) - 1;

const STI_SHIFT: u32 = 24;
const STI_BITS: u32 = 12;
const STI_MASK: u64 = (1 << STI_BITS) - 1;

const OWN_LEN_SHIFT: u32 = 36;
const OWN_LEN_BITS: u32 = 14;
const OWN_LEN_MASK: u64 = (1 << OWN_LEN_BITS) - 1;

const SEP_LEN_SHIFT: u32 = 50;
const SEP_LEN_BITS: u32 = 8;
const SEP_LEN_MASK: u64 = (1 << SEP_LEN_BITS) - 1;

const OVERLAP_SHIFT: u32 = 58;
const OVERLAP_BITS: u32 = 4;
const OVERLAP_MASK: u64 = (1 << OVERLAP_BITS) - 1;

/// Most overlap bytes a parent record carries (the collector borrows 2; the
/// 4-bit `overlap_len` field could say 15, the builder refuses above this).
pub const MAX_OVERLAP_BYTES: usize = 4;

/// A parent entry with v3 metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParentEntryV3 {
    /// Ordinal of the token this suffix belongs to (24 bits in the single-parent encoding).
    pub raw_ordinal: u64,
    /// Suffix start index: byte offset of this suffix within the extended token.
    pub sti: u16,
    /// Bytes owned by the token: content + trailing separators, no overlap.
    pub own_len: u16,
    /// Trailing separator bytes included in `own_len`.
    pub sep_len: u8,
    /// Bytes borrowed from the next token at the end of the extended text.
    pub overlap_len: u8,
    /// Those bytes themselves (the first `overlap_len` are meaningful), lower
    /// case. Written since container version 7, where the key stops at the
    /// token boundary; zero in a record read from an older file, whose keys
    /// carry the overlap themselves.
    pub overlap: [u8; MAX_OVERLAP_BYTES],
    /// True when the token is the first chunk of a word.
    pub is_word_start: bool,
}

impl ParentEntryV3 {
    /// Content length = own_len - sep_len (alphanumeric bytes only).
    pub fn content_len(&self) -> u16 {
        self.own_len - self.sep_len as u16
    }
}

/// Encode a single-parent v3 value into u64.
pub fn encode_single_parent_v3(p: &ParentEntryV3) -> u64 {
    debug_assert!(p.raw_ordinal <= ORDINAL_MASK, "ordinal overflow: {}", p.raw_ordinal);
    debug_assert!((p.sti as u64) <= STI_MASK, "STI overflow: {}", p.sti);
    debug_assert!((p.own_len as u64) <= OWN_LEN_MASK, "own_len overflow: {}", p.own_len);
    debug_assert!((p.sep_len as u64) <= SEP_LEN_MASK, "sep_len overflow: {}", p.sep_len);
    debug_assert!((p.overlap_len as u64) <= OVERLAP_MASK, "overlap_len overflow: {}", p.overlap_len);

    let mut val = p.raw_ordinal & ORDINAL_MASK;
    val |= (p.sti as u64) << STI_SHIFT;
    val |= (p.own_len as u64) << OWN_LEN_SHIFT;
    val |= (p.sep_len as u64) << SEP_LEN_SHIFT;
    val |= (p.overlap_len as u64) << OVERLAP_SHIFT;
    if p.is_word_start {
        val |= WORD_START_FLAG;
    }
    val
}

/// Encode a multi-parent offset.
pub fn encode_multi_parent_v3(offset: u64) -> u64 {
    MULTI_FLAG | offset
}

/// Decoded v3 parent reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParentRefV3 {
    /// One parent, decoded inline from the 64-bit FST output.
    Single(ParentEntryV3),
    /// Several parents, stored as a record in the OutputTable.
    Multi {
        /// Offset of the encoded parent list in the OutputTable.
        offset: u64,
    },
}

/// Decode a v3 u64 FST output value.
pub fn decode_output_v3(value: u64) -> ParentRefV3 {
    if value & MULTI_FLAG != 0 {
        ParentRefV3::Multi {
            offset: value & !MULTI_FLAG,
        }
    } else {
        ParentRefV3::Single(ParentEntryV3 {
            raw_ordinal: value & ORDINAL_MASK,
            sti: ((value >> STI_SHIFT) & STI_MASK) as u16,
            own_len: ((value >> OWN_LEN_SHIFT) & OWN_LEN_MASK) as u16,
            sep_len: ((value >> SEP_LEN_SHIFT) & SEP_LEN_MASK) as u8,
            overlap_len: ((value >> OVERLAP_SHIFT) & OVERLAP_MASK) as u8,
            overlap: [0; MAX_OVERLAP_BYTES],
            is_word_start: value & WORD_START_FLAG != 0,
        })
    }
}

/// Encode a multi-parent record for the OutputTable (container version 5).
///
/// Header: varint count. Parents sorted by (ordinal, sti); per parent:
/// `[varint ordinal - previous ordinal][varint sti][varint own_len][u8 sep_len]
/// [u8 flags]`, flags = `overlap_len` (4 bits) | `is_word_start` (bit 4).
/// About 5 bytes per parent against the 8 of version 4 and the 11 of
/// version 3 — and the largest lists compress best: the 54 747 chunks ending
/// in `_` on a 30 000-file kernel segment are spread over 1.2 million
/// ordinals, so their deltas fit in one byte.
///
/// Parents used to be sorted by `sti`; no reader depended on it
/// (`sort_and_dedup_splits` sorts what it keeps).
pub fn encode_parent_entries_v3(parents: &[ParentEntryV3]) -> Vec<u8> {
    let mut sorted = parents.to_vec();
    sorted.sort_by_key(|p| (p.raw_ordinal, p.sti));
    encode_sorted_parent_entries_v3(sorted.iter())
}

/// Version-8 record. The key stops at the token boundary since version 7,
/// so one key gathers every chunk with that own text whatever follows it
/// (`_` ends 54 747 chunks on a kernel segment) and the overlap bytes live
/// here. A large record is **grouped by overlap**, so that a walk standing
/// at a boundary reads only the group whose overlap agrees with the query
/// and skips the others unread; a small one is a flat list, cheaper than
/// its group headers would be.
///
/// Header byte: bit 7 = grouped; bits 6..0 = the parent count (flat) or
/// the group count (grouped), 0 meaning a varint follows.
/// A parent is `[varint Δordinal][varint sti][u8 flags]`, then `[varint
/// own_len]` only when flags say so, `[varint sep_len]` only when the
/// three-bit field is 7, and in a flat record its overlap bytes. Flags:
/// bit 7 `is_word_start`, bit 6 explicit `own_len`, bits 5..3 overlap
/// length, bits 2..0 `sep_len` (7 = a varint follows).
/// `own_len` is otherwise **derived from the key**: the key is the token's
/// own bytes from `sti`, so `own_len = key body length + sti` for a chunk,
/// plus `sep_len` for a word entry (partition 0x02, whose key is the
/// content only). It is written out when lowercasing changed a byte
/// length and the two disagree. Decoding therefore takes the key.
/// Grouped: per group, in overlap order, `[u8 overlap_len][overlap bytes]
/// [varint parents][zigzag Δfirst][varint byte_len unless last group]`,
/// where Δfirst is the group's first ordinal against the previous group's
/// first, then its parents as above without the overlap, each ordinal a
/// delta against the previous parent of the group (the first against the
/// group's first ordinal, so 0). A group is skipped by `byte_len` unread.
pub const FLAT_RECORD_MAX_PARENTS: usize = 32;

const FLAG_WORD_START: u8 = 0x80;
const FLAG_OWN_LEN: u8 = 0x40;
const SEP_LEN_ESCAPE: u8 = 7;

/// Encode a version-8 record for the parents of `key` (partition byte first).
pub fn encode_parent_entries_v8(parents: &[ParentEntryV3], key: &[u8]) -> Vec<u8> {
    let mut sorted = parents.to_vec();
    encode_parent_record_v8(&mut sorted, key)
}

fn zigzag(delta: i64) -> u64 {
    ((delta << 1) ^ (delta >> 63)) as u64
}

fn unzigzag(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

/// `own_len` as the key implies it (see the record layout).
#[inline(always)]
fn derived_own_len(key: &[u8], sti: u16, sep_len: u8) -> u16 {
    let body = key.len().saturating_sub(1) as u16;
    if key.first() == Some(&SI_STRIPPED_PREFIX) { body + sti + sep_len as u16 } else { body + sti }
}

fn write_parent_fields(buf: &mut Vec<u8>, p: &ParentEntryV3, key: &[u8], with_overlap: bool) {
    use super::varint::write_varint;
    write_varint(buf, p.sti as u64);
    let explicit = derived_own_len(key, p.sti, p.sep_len) != p.own_len;
    let ov_len = (p.overlap_len as usize).min(MAX_OVERLAP_BYTES) as u8;
    let mut flags = (ov_len << 3) | if p.sep_len < SEP_LEN_ESCAPE { p.sep_len } else { SEP_LEN_ESCAPE };
    if p.is_word_start { flags |= FLAG_WORD_START; }
    if explicit { flags |= FLAG_OWN_LEN; }
    buf.push(flags);
    if explicit { write_varint(buf, p.own_len as u64); }
    if p.sep_len >= SEP_LEN_ESCAPE { write_varint(buf, p.sep_len as u64); }
    if with_overlap { buf.extend_from_slice(&p.overlap[..ov_len as usize]); }
}

/// `encode_parent_entries_v8` over a scratch vector it may reorder.
pub fn encode_parent_record_v8(parents: &mut [ParentEntryV3], key: &[u8]) -> Vec<u8> {
    use super::varint::write_varint;
    fn overlap_of(p: &ParentEntryV3) -> &[u8] {
        &p.overlap[..(p.overlap_len as usize).min(MAX_OVERLAP_BYTES)]
    }
    let mut buf = Vec::with_capacity(4 + parents.len() * 6);
    if parents.len() <= FLAT_RECORD_MAX_PARENTS {
        parents.sort_by_key(|p| (p.raw_ordinal, p.sti));
        buf.push(parents.len() as u8);
        let mut prev = 0u64;
        for p in parents.iter() {
            write_varint(&mut buf, p.raw_ordinal - prev);
            prev = p.raw_ordinal;
            write_parent_fields(&mut buf, p, key, true);
        }
        return buf;
    }
    parents.sort_by(|a, b| {
        overlap_of(a).cmp(overlap_of(b))
            .then(a.raw_ordinal.cmp(&b.raw_ordinal))
            .then(a.sti.cmp(&b.sti))
    });
    let mut groups: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < parents.len() {
        let mut j = i + 1;
        while j < parents.len() && overlap_of(&parents[j]) == overlap_of(&parents[i]) { j += 1; }
        groups.push((i, j));
        i = j;
    }
    if groups.len() <= 127 {
        buf.push(0x80 | groups.len() as u8);
    } else {
        buf.push(0x80);
        write_varint(&mut buf, groups.len() as u64);
    }
    let mut body = Vec::new();
    let mut prev_first = 0i64;
    for (g, &(i, j)) in groups.iter().enumerate() {
        let ov = overlap_of(&parents[i]);
        buf.push(ov.len() as u8);
        buf.extend_from_slice(ov);
        write_varint(&mut buf, (j - i) as u64);
        let first = parents[i].raw_ordinal as i64;
        write_varint(&mut buf, zigzag(first - prev_first));
        prev_first = first;
        body.clear();
        let mut prev = first as u64;
        for p in &parents[i..j] {
            write_varint(&mut body, p.raw_ordinal - prev);
            prev = p.raw_ordinal;
            write_parent_fields(&mut body, p, key, false);
        }
        if g + 1 < groups.len() {
            write_varint(&mut buf, body.len() as u64);
        }
        buf.extend_from_slice(&body);
    }
    buf
}

/// `encode_parent_entries_v3` over parents already sorted by `(ordinal, sti)`;
/// the builder's groups are, and it encodes millions of them.
pub fn encode_sorted_parent_entries_v3<'a>(
    parents: impl ExactSizeIterator<Item = &'a ParentEntryV3>,
) -> Vec<u8> {
    use super::varint::write_varint;
    let mut buf = Vec::with_capacity(5 + parents.len() * 5);
    write_varint(&mut buf, parents.len() as u64);
    let mut prev = 0u64;
    for p in parents {
        debug_assert!(p.raw_ordinal >= prev, "parents must be sorted by ordinal");
        debug_assert!((p.overlap_len as u64) <= OVERLAP_MASK);
        write_varint(&mut buf, p.raw_ordinal - prev);
        prev = p.raw_ordinal;
        write_varint(&mut buf, p.sti as u64);
        write_varint(&mut buf, p.own_len as u64);
        buf.push(p.sep_len);
        buf.push((p.overlap_len & OVERLAP_MASK as u8) | if p.is_word_start { 0x10 } else { 0 });
    }
    buf
}

/// Decode a version-5 multi-parent record (see `encode_parent_entries_v3`).
///
/// Sequential; the varint reads are inlined because this runs once per
/// final node of every walk, on lists of up to tens of thousands.
pub fn decode_parent_entries_v3(data: &[u8]) -> Vec<ParentEntryV3> {
    let mut pos = 0usize;
    let num = read_varint_inline(data, &mut pos) as usize;
    let mut entries = Vec::with_capacity(num);
    decode_parent_run(data, &mut pos, num, None, &mut entries);
    entries
}

/// Decode every parent of a version-8 record under `key` (see `encode_parent_entries_v8`).
pub fn decode_parent_entries_v8(data: &[u8], key: &[u8]) -> Vec<ParentEntryV3> {
    decode_parent_entries_v8_where(data, key, |_| true)
}

/// Decode the parents of a version-8 record whose overlap bytes satisfy
/// `keep`; in a grouped record the other groups are skipped unread.
pub fn decode_parent_entries_v8_where(data: &[u8], key: &[u8], keep: impl Fn(&[u8]) -> bool) -> Vec<ParentEntryV3> {
    let head = data[0];
    let mut pos = 1usize;
    let count = match head & 0x7F { 0 => read_varint_inline(data, &mut pos) as usize, k => k as usize };
    let mut entries = Vec::new();
    if head & 0x80 == 0 {
        // Flat: every parent carries its overlap.
        entries.reserve(count);
        let mut ordinal = 0u64;
        for _ in 0..count {
            ordinal += read_varint_inline(data, &mut pos);
            let (sti, own_len, sep_len, flags) = read_parent_fields(data, &mut pos, key);
            let ov_len = ((flags >> 3) & 0x07) as usize;
            let mut overlap = [0u8; MAX_OVERLAP_BYTES];
            overlap[..ov_len].copy_from_slice(&data[pos..pos + ov_len]);
            pos += ov_len;
            if keep(&overlap[..ov_len]) {
                entries.push(ParentEntryV3 {
                    raw_ordinal: ordinal, sti, own_len, sep_len,
                    overlap_len: ov_len as u8, overlap, is_word_start: flags & FLAG_WORD_START != 0,
                });
            }
        }
        return entries;
    }
    let mut prev_first = 0i64;
    for g in 0..count {
        let ov_len = (data[pos] as usize).min(MAX_OVERLAP_BYTES);
        pos += 1;
        let mut overlap = [0u8; MAX_OVERLAP_BYTES];
        overlap[..ov_len].copy_from_slice(&data[pos..pos + ov_len]);
        pos += ov_len;
        let n = read_varint_inline(data, &mut pos) as usize;
        prev_first += unzigzag(read_varint_inline(data, &mut pos));
        let byte_len = if g + 1 < count { read_varint_inline(data, &mut pos) as usize } else { 0 };
        if keep(&overlap[..ov_len]) {
            entries.reserve(n);
            let mut ordinal = prev_first as u64;
            for _ in 0..n {
                ordinal += read_varint_inline(data, &mut pos);
                let (sti, own_len, sep_len, flags) = read_parent_fields(data, &mut pos, key);
                entries.push(ParentEntryV3 {
                    raw_ordinal: ordinal, sti, own_len, sep_len,
                    overlap_len: ov_len as u8, overlap, is_word_start: flags & FLAG_WORD_START != 0,
                });
            }
        } else if g + 1 < count {
            pos += byte_len;
        } else {
            break;
        }
    }
    entries
}

/// `(sti, own_len, sep_len, flags)` of one parent; `own_len` derived from
/// `key` unless the record spells it out.
#[inline(always)]
fn read_parent_fields(data: &[u8], pos: &mut usize, key: &[u8]) -> (u16, u16, u8, u8) {
    let sti = read_varint_inline(data, pos) as u16;
    let flags = data[*pos];
    *pos += 1;
    let mut own_len = 0u16;
    if flags & FLAG_OWN_LEN != 0 {
        own_len = read_varint_inline(data, pos) as u16;
    }
    let sep_len = match flags & 0x07 {
        SEP_LEN_ESCAPE => read_varint_inline(data, pos) as u8,
        k => k,
    };
    if flags & FLAG_OWN_LEN == 0 {
        own_len = derived_own_len(key, sti, sep_len);
    }
    (sti, own_len, sep_len, flags)
}

/// Sequential varint read; inlined because the decoders run once per final
/// node of every walk, on lists of up to tens of thousands.
#[inline(always)]
fn read_varint_inline(data: &[u8], pos: &mut usize) -> u64 {
    let b = data[*pos];
    *pos += 1;
    if b < 0x80 {
        return b as u64;
    }
    let mut v = (b & 0x7f) as u64;
    let mut shift = 7;
    loop {
        let b = data[*pos];
        *pos += 1;
        v |= ((b & 0x7f) as u64) << shift;
        if b < 0x80 {
            return v;
        }
        shift += 7;
    }
}

/// `n` delta-coded parents from `pos`. With `group = None` the flags byte
/// carries the overlap length (versions 5 and 6); with `Some` the group's
/// overlap applies (version 7).
#[inline(always)]
fn decode_parent_run(
    data: &[u8],
    pos: &mut usize,
    n: usize,
    group: Option<(u8, [u8; MAX_OVERLAP_BYTES])>,
    entries: &mut Vec<ParentEntryV3>,
) {
    let mut ordinal = 0u64;
    for _ in 0..n {
        ordinal += read_varint_inline(data, pos);
        let sti = read_varint_inline(data, pos) as u16;
        let own_len = read_varint_inline(data, pos) as u16;
        let sep_len = data[*pos];
        let flags = data[*pos + 1];
        *pos += 2;
        let (overlap_len, overlap) = match group {
            Some(g) => g,
            None => (flags & OVERLAP_MASK as u8, [0; MAX_OVERLAP_BYTES]),
        };
        entries.push(ParentEntryV3 {
            raw_ordinal: ordinal,
            sti,
            own_len,
            sep_len,
            overlap_len,
            overlap,
            is_word_start: flags & 0x10 != 0,
        });
    }
}

/// Decode a version-4 multi-parent record: varint count, then the packed
/// 8-byte parent value (`encode_single_parent_v3`) per parent. Written
/// between the morning and the afternoon of 4 September 2026; still read.
pub fn decode_parent_entries_v4_packed(data: &[u8]) -> Vec<ParentEntryV3> {
    let mut cursor = 0usize;
    let num = super::varint::read_varint(data, &mut cursor)
        .expect("BUG: truncated parent record header") as usize;
    let mut entries = Vec::with_capacity(num);
    for chunk in data[cursor..].chunks_exact(8).take(num) {
        let value = u64::from_le_bytes(chunk.try_into().expect("8-byte chunk"));
        match decode_output_v3(value) {
            ParentRefV3::Single(p) => entries.push(p),
            ParentRefV3::Multi { .. } => unreachable!("BUG: multi flag inside a parent record"),
        }
    }
    entries
}

/// Decode a version-3 multi-parent record: `[u32 count]` then 11 bytes per
/// entry — `[u32 ordinal][u16 sti][u16 own_len][u8 sep_len][u8 overlap_len][u8 flags]`.
///
/// Kept so that every index written before container version 4 still opens;
/// nothing writes this shape any more. (The count was a u16 until 23 August
/// 2026: a 50k-document kernel index merged into one segment reached 64 461
/// parents under one key, and the next merge would have truncated the list.)
pub fn decode_parent_entries_v3_legacy(data: &[u8]) -> Vec<ParentEntryV3> {
    let num = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
    let mut cursor = 4;
    let mut entries = Vec::with_capacity(num);
    for _ in 0..num {
        let raw_ordinal = u32::from_le_bytes([
            data[cursor], data[cursor + 1], data[cursor + 2], data[cursor + 3],
        ]) as u64;
        cursor += 4;
        let sti = u16::from_le_bytes([data[cursor], data[cursor + 1]]);
        cursor += 2;
        let own_len = u16::from_le_bytes([data[cursor], data[cursor + 1]]);
        cursor += 2;
        let sep_len = data[cursor];
        cursor += 1;
        let overlap_len = data[cursor];
        cursor += 1;
        let is_word_start = data[cursor] != 0;
        cursor += 1;
        entries.push(ParentEntryV3 {
            raw_ordinal, sti, own_len, sep_len, overlap_len, overlap: [0; MAX_OVERLAP_BYTES], is_word_start,
        });
    }
    entries
}

// ─── Builder ───────────────────────────────────────────────────────────────

/// V3 suffix FST builder with overlap support.
///
/// The caller (collector) is responsible for:
/// 1. Tokenizing with `EqualChunkTokenizer`
/// 2. Computing overlap bytes (min(2, next_token.len()) bytes from next token)
/// 3. Calling `add_token` with the extended bytes and metadata
pub struct SuffixFstBuilderV3 {
    key_buf: Vec<u8>,
    entries: Vec<(u32, u32, ParentEntryV3)>,
    min_suffix_len: usize,
    num_terms: usize,
    max_parents: usize,
}

impl Default for SuffixFstBuilderV3 {
    fn default() -> Self {
        Self::new()
    }
}

impl SuffixFstBuilderV3 {
    /// Empty builder; the minimum suffix length comes from
    /// `LUCIVY_MIN_SUFFIX_LEN` (default 1).
    pub fn new() -> Self {
        Self::with_min_suffix_len(default_min_suffix_len())
    }

    /// Empty builder that stops generating SI>0 suffixes shorter than `min` bytes.
    pub fn with_min_suffix_len(min: usize) -> Self {
        Self {
            key_buf: Vec::new(),
            entries: Vec::new(),
            min_suffix_len: min,
            num_terms: 0,
            max_parents: 0,
        }
    }

    /// Register all suffixes of an extended token (content + sep + overlap).
    ///
    /// `extended_token` = the full string to index (will be lowercased internally).
    /// The metadata fields describe the structure within those bytes.
    ///
    /// Suffixes are generated over the full extended token (including overlap),
    /// but own_len in the encoding excludes the overlap — so the falling walk
    /// knows where the token boundary is.
    /// Register all suffixes of an extended token.
    ///
    /// `content_overlap` (optional): for partition 0x02 (sep-stripped), use these
    /// bytes instead of the normal overlap. This is the content-aware overlap that
    /// skips pure-sep tokens and takes bytes from the next CONTENT token.
    /// When None, stripped entries use the normal overlap (from extended_token).
    pub fn add_token(
        &mut self,
        extended_token: &str,
        raw_ordinal: u64,
        own_len: u16,
        sep_len: u8,
        overlap_len: u8,
        is_word_start: bool,
    ) {
        self.add_token_with_content_overlap(
            extended_token, raw_ordinal, own_len, sep_len, overlap_len, is_word_start, None,
        );
    }

    /// Like `add_token` but with explicit content-aware overlap for stripped partition.
    pub fn add_token_with_content_overlap(
        &mut self,
        extended_token: &str,
        raw_ordinal: u64,
        own_len: u16,
        sep_len: u8,
        overlap_len: u8,
        is_word_start: bool,
        content_overlap: Option<&str>,
    ) {
        // The token's own bytes and its overlap are lowercased separately:
        // lowercasing can change a byte length (`İ` is two bytes as `i̇`),
        // so cutting the lowercased text at `own_len` would put own bytes
        // in the overlap or the reverse. `own_len` and `sti` stay what the
        // collector measured on the original text, as they always did.
        let mut split = extended_token.len().saturating_sub(overlap_len as usize);
        while split > 0 && !extended_token.is_char_boundary(split) {
            split -= 1;
        }
        let lower = extended_token[..split].to_lowercase();
        let lower_overlap = extended_token[split..].to_lowercase();
        let extended_bytes = lower.as_bytes();
        let own_end = extended_bytes.len();
        // Suffixes start inside the token's own bytes only. A suffix starting
        // in the overlap (`si >= own_len`) is one or two bytes of the NEXT
        // token, which carries them itself under its own ordinal (at sti 0 and
        // 1 — the same text position, in the same value); the walk rejected
        // such parents (`fst_walk::check_split`) and the range scan resolved
        // them to duplicates of the next token's spans. They were the 1- and
        // 2-byte keys, i.e. the ones with the largest parent lists — up to
        // 317 000 parents under one key on the kernel corpus.
        let max_si = own_end.min(MAX_CHUNK_BYTES);
        // The key stops at the token boundary (container version 7): the
        // overlap bytes go into the parent record instead, right after the
        // key in `key_buf` until `build()` encodes them. Two chunks with the
        // same own text and different overlaps now share a key, and the
        // "marker" key that used to be cut at the boundary for the walk is
        // the key itself. Keys shrank 63 to 81 % on kernel segments. The
        // record holds at most `MAX_OVERLAP_BYTES`; a longer lowercase form
        // is cut at a character boundary rather than refused.
        let mut ov_end = lower_overlap.len().min(MAX_OVERLAP_BYTES);
        while ov_end > 0 && !lower_overlap.is_char_boundary(ov_end) {
            ov_end -= 1;
        }
        let overlap_bytes = &lower_overlap.as_bytes()[..ov_end];
        let overlap_len = overlap_bytes.len() as u8;

        // ── Normal suffixes (partitions 0x00 and 0x01) ──
        for si in 0..max_si {
            if si > 0 && !is_utf8_char_boundary(extended_bytes, si) {
                continue;
            }
            // The minimum suffix length counts the overlap, as it always did.
            if si > 0 && (own_end - si) + overlap_bytes.len() < self.min_suffix_len {
                break;
            }

            let prefix = if si == 0 { SI0_PREFIX } else { SI_REST_PREFIX };
            let key_start = self.key_buf.len() as u32;
            self.key_buf.push(prefix);
            self.key_buf.extend_from_slice(&extended_bytes[si..own_end]);
            let key_len = (self.key_buf.len() as u32) - key_start;
            self.key_buf.extend_from_slice(overlap_bytes);

            self.entries.push((key_start, key_len, ParentEntryV3 {
                raw_ordinal,
                sti: si as u16,
                own_len,
                sep_len,
                overlap_len,
                overlap: [0; MAX_OVERLAP_BYTES],
                is_word_start,
            }));
        }

        // NOTE: stripped partition (0x02) is now word-level, generated via add_word_stripped().
        // Per-chunk stripped entries are no longer generated here.
        let _ = content_overlap; // consumed by caller for word-level stripped
    }

    /// Register word-level stripped suffixes in partition 0x02.
    ///
    /// `word_content` = concatenation of all content bytes of the word's chunks (no seps).
    /// `content_overlap` = first 2 bytes of the next CONTENT token (from next word).
    /// `first_ordinal` = ordinal of the first chunk of this word (for posting resolution).
    /// `first_own_len` = own_len of the first chunk.
    ///
    /// This indexes suffixes of the ENTIRE word (not per-chunk), so queries like
    /// "nationalizationinit" that span multiple chunks within a word are directly
    /// findable in the FST without multi-hop chaining.
    pub fn add_word_stripped(
        &mut self,
        word_content: &str,
        content_overlap: &str,
        first_ordinal: u64,
        _first_own_len: u16,
        first_sep_len: u8,
        is_word_start: bool,
    ) {
        let lower_content = word_content.to_lowercase();
        let lower_overlap = content_overlap.to_lowercase();
        let content_bytes = lower_content.as_bytes();
        let mut ov_end = lower_overlap.len().min(MAX_OVERLAP_BYTES);
        while ov_end > 0 && !lower_overlap.is_char_boundary(ov_end) {
            ov_end -= 1;
        }
        let overlap_bytes = &lower_overlap.as_bytes()[..ov_end];
        let content_len = content_bytes.len();

        if content_len == 0 {
            return;
        }
        // A word without a trailing separator (the last word of a value, or
        // a value that is one word) is indexed like any other. It used to be
        // skipped — "the chunks already cover it" — which held only while
        // relaxed queries also walked the chunk chains. Since those chains
        // are skipped when the segment has no long word (B2 bis, 23 August),
        // the word partition must hold every word: `rag3weaver` at the end
        // of a value, chunked `rag3w|eaver`, was unreachable for `weaver`.
        // Clamp: own_len = content_len + sep_len must fit in 14 bits (max 16383).
        // For very long words (e.g. base64), only index the first portion.
        let effective_content_len = content_len.min(OWN_LEN_MASK as usize - first_sep_len as usize);
        let content_bytes = &content_bytes[..effective_content_len];
        let content_len = effective_content_len;

        let max_si = content_len.min(MAX_CHUNK_BYTES);

        for si in 0..max_si {
            if si > 0 && !is_utf8_char_boundary(content_bytes, si) {
                continue;
            }
            let suffix_content = &content_bytes[si..];
            let suffix_len = suffix_content.len() + overlap_bytes.len();
            if si > 0 && suffix_len < self.min_suffix_len {
                break;
            }

            // Key = the word's content from `si`; the content overlap follows
            // it in `key_buf` for the record (see `add_token_with_content_overlap`).
            let key_start = self.key_buf.len() as u32;
            self.key_buf.push(SI_STRIPPED_PREFIX);
            self.key_buf.extend_from_slice(suffix_content);
            let key_len = (self.key_buf.len() as u32) - key_start;
            self.key_buf.extend_from_slice(overlap_bytes);

            let parent = ParentEntryV3 {
                raw_ordinal: first_ordinal,
                sti: si as u16,
                own_len: (content_len + first_sep_len as usize) as u16,
                sep_len: first_sep_len,
                overlap_len: overlap_bytes.len() as u8,
                overlap: [0; MAX_OVERLAP_BYTES],
                is_word_start,
            };

            self.entries.push((key_start, key_len, parent));
        }
    }

    /// Build the FST and output table bytes.
    pub fn build(mut self) -> Result<(Vec<u8>, Vec<u8>), lucivy_fst::Error> {
        // Sort on a cached 8-byte prefix first.
        //
        // Every comparison used to be a memcmp behind two random reads into
        // key_buf — around 100 million of them, two cache misses each, on a
        // merged segment with 4.7 million entries. It was the single largest
        // cost of a merge. Nearly all of them separate on the first few bytes.
        //
        // The prefix is the first up-to-8 key bytes, zero-padded, read big-endian,
        // so its numeric order is the keys' lexicographic order: a shorter key
        // pads with zeros and sorts before any key extending it. Keys that agree
        // on those 8 bytes — including keys holding real zero bytes, which the
        // partition prefixes 0x00/0x01/0x02 guarantee — fall through to the full
        // slice comparison, so the order is unchanged, not merely close.
        fn prefix8(buf: &[u8], start: u32, len: u32) -> u64 {
            let s = start as usize;
            let n = (len as usize).min(8);
            let mut p = [0u8; 8];
            p[..n].copy_from_slice(&buf[s..s + n]);
            u64::from_be_bytes(p)
        }

        let _t_sort = std::time::Instant::now();
        let n_entries = self.entries.len();
        let buf = &self.key_buf;
        let mut keyed: Vec<(u64, u32, u32, ParentEntryV3)> = std::mem::take(&mut self.entries)
            .into_iter()
            .map(|(start, len, parent)| (prefix8(buf, start, len), start, len, parent))
            .collect();

        keyed.sort_unstable_by(|a, b| {
            a.0.cmp(&b.0)
                .then_with(|| buf[a.1 as usize..(a.1 + a.2) as usize]
                    .cmp(&buf[b.1 as usize..(b.1 + b.2) as usize]))
                .then(a.3.raw_ordinal.cmp(&b.3.raw_ordinal))
                .then(a.3.sti.cmp(&b.3.sti))
        });
        keyed.dedup_by(|a, b| {
            a.0 == b.0
                && buf[a.1 as usize..(a.1 + a.2) as usize] == buf[b.1 as usize..(b.1 + b.2) as usize]
                && a.3.raw_ordinal == b.3.raw_ordinal
                && a.3.sti == b.3.sti
        });

        self.entries = keyed.into_iter().map(|(_, s, l, p)| (s, l, p)).collect();
        let ns_sort = _t_sort.elapsed().as_nanos();
        let _t_rest = std::time::Instant::now();

        let mut fst_builder = MapBuilder::memory();
        let mut output_table = OutputTableBuilder::new();
        let mut scratch: Vec<ParentEntryV3> = Vec::new();
        self.num_terms = 0;

        // Diagnostics: multi-parent stats
        let do_diag = std::env::var("V3_DIAG_BUILD").is_ok();
        let mut diag_file = if do_diag {
            std::fs::OpenOptions::new().create(true).append(true)
                .open("/tmp/v3_diag_build.txt").ok()
        } else { None };
        let mut multi_parent_count = 0u64;
        let mut max_parents = 0usize;
        let mut multi_parent_distinct_ords = 0u64;
        let mut max_ordinal = 0u64;

        let mut i = 0;
        while i < self.entries.len() {
            let (ks, kl, _) = self.entries[i];
            let key = &buf[ks as usize..(ks + kl) as usize];

            let mut j = i + 1;
            while j < self.entries.len() {
                let (js, jl, _) = self.entries[j];
                if &buf[js as usize..(js + jl) as usize] != key { break; }
                j += 1;
            }
            let num_parents = j - i;

            // Diag: log multi-parent keys with distinct ordinals
            if num_parents > 1 {
                multi_parent_count += 1;
                if num_parents > max_parents { max_parents = num_parents; }
                let mut ords: Vec<u64> = self.entries[i..j].iter().map(|e| e.2.raw_ordinal).collect();
                ords.sort_unstable();
                ords.dedup();
                if ords.len() > 1 {
                    multi_parent_distinct_ords += 1;
                    if let Some(ref mut f) = diag_file {
                        use std::io::Write;
                        let key_str = String::from_utf8_lossy(&key[1..]); // skip partition prefix
                        let partition = key[0];
                        let parents_info: Vec<String> = self.entries[i..j].iter()
                            .map(|e| format!("ord={} sti={} own={} sep={}",
                                e.2.raw_ordinal, e.2.sti, e.2.own_len, e.2.sep_len))
                            .collect();
                        writeln!(f, "MULTI_ORD partition=0x{:02x} key={:?} parents=[{}]",
                            partition, &key_str[..key_str.len().min(30)],
                            parents_info.join(", ")).ok();
                    }
                }
            }

            // Hard errors, not debug_asserts: release builds disable those
            // (Cargo.toml), and past either limit the index is silently wrong —
            // a term serving another term's postings, or parents dropped and
            // occurrences lost. Merges are exactly the operation that crosses
            // these thresholds.
            // The record encodes the ordinal as a varint since version 5, so
            // the bound is no longer this file's: `MAX_ORDINAL` is what
            // `.word_pos_map`'s slot holds (28 bits), and `.posmap` widens
            // itself past 24. A hard error, not a debug_assert: past it the
            // word map would silently vanish.
            if num_parents > self.max_parents { self.max_parents = num_parents; }
            for e in &self.entries[i..j] {
                let p = &e.2;
                if p.raw_ordinal > Self::MAX_ORDINAL {
                    return Err(std::io::Error::other(format!(
                        "sfx v3: ordinal {} exceeds the {} ordinals a segment can address; \
                         the segment holds too many distinct terms (split it instead of merging)",
                        p.raw_ordinal, Self::MAX_ORDINAL + 1)).into());
                }
                if (p.overlap_len as usize) > MAX_OVERLAP_BYTES {
                    return Err(std::io::Error::other(format!(
                        "sfx v3: overlap of {} bytes exceeds the {MAX_OVERLAP_BYTES} a parent record holds",
                        p.overlap_len)).into());
                }
                if (p.own_len as u64) > OWN_LEN_MASK || (p.sti as u64) > STI_MASK {
                    return Err(std::io::Error::other(format!(
                        "sfx v3: own_len {} / sti {} exceed the parent encoding", p.own_len, p.sti)).into());
                }
                if p.raw_ordinal > max_ordinal { max_ordinal = p.raw_ordinal; }
            }

            // Every key, single parent included, points at a record: the
            // offsets grow with the keys, so the FST shares them along its
            // paths instead of carrying an incompressible 8-byte value under
            // each final node. The group is already sorted by (ordinal, sti).
            // The overlap bytes sit after each key in `key_buf`; copy them
            // into the parents now that the group is final.
            for e in &mut self.entries[i..j] {
                let n = (e.2.overlap_len as usize).min(MAX_OVERLAP_BYTES);
                let from = (e.0 + e.1) as usize;
                e.2.overlap[..n].copy_from_slice(&buf[from..from + n]);
            }
            scratch.clear();
            scratch.extend(self.entries[i..j].iter().map(|e| e.2.clone()));
            let record = encode_parent_record_v8(&mut scratch, key);
            let offset = output_table.add(&record);
            fst_builder.insert(key, offset)?;
            self.num_terms += 1;

            i = j;
        }

        if do_diag {
            if let Some(ref mut f) = diag_file {
                use std::io::Write;
                writeln!(f, "\n=== SUMMARY: {} total keys, {} multi-parent, {} with distinct ordinals, max_parents={} ===\n",
                    self.num_terms, multi_parent_count, multi_parent_distinct_ords, max_parents).ok();
            }
        }

        let fst_bytes = fst_builder.into_inner()?;
        if crate::suffix_fst::briques::profile::enabled() {
            eprintln!("  [fst] {} entries, key_buf {}KB | sort {:.0}ms | build+serialize {:.0}ms | fst {}KB | {} keys, max_parents {}, ordinal headroom {:.1}%",
                n_entries, buf.len() / 1024,
                ns_sort as f64 / 1e6,
                _t_rest.elapsed().as_nanos() as f64 / 1e6,
                fst_bytes.len() / 1024,
                self.num_terms, self.max_parents,
                100.0 * max_ordinal as f64 / Self::MAX_ORDINAL as f64);
        }
        Ok((fst_bytes, output_table.into_inner()))
    }

    /// Number of distinct keys inserted into the FST by the last `build()`.
    pub fn num_terms(&self) -> usize {
        self.num_terms
    }

    /// Largest parent list under one key in the last `build()`.
    pub fn max_parents(&self) -> usize {
        self.max_parents
    }

    /// Largest ordinal a v3 segment can address: what a `.word_pos_map`
    /// slot holds (28 bits since 4 September 2026 at night; the `.sfx`
    /// record is a varint and `.posmap` widens itself). `build()` and
    /// `merge_segments_v3` refuse beyond it.
    pub const MAX_ORDINAL: u64 = (1 << super::word_pos_map::SLOT_ORDINAL_BITS) - 1;
}

/// Check if position `i` is a UTF-8 char boundary in a byte slice.
fn is_utf8_char_boundary(bytes: &[u8], i: usize) -> bool {
    if i >= bytes.len() { return true; }
    // A byte is a char boundary if it does NOT start with 0b10xxxxxx
    (bytes[i] & 0b1100_0000) != 0b1000_0000
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Encoding round-trip ──

    #[test]
    fn test_encode_decode_single() {
        let entry = ParentEntryV3 {
            raw_ordinal: 42,
            sti: 3,
            own_len: 8,
            sep_len: 1,
            overlap_len: 2,
            overlap: [0; MAX_OVERLAP_BYTES],
            is_word_start: true,
        };
        let val = encode_single_parent_v3(&entry);
        match decode_output_v3(val) {
            ParentRefV3::Single(p) => assert_eq!(p, entry),
            _ => panic!("expected single"),
        }
    }

    #[test]
    fn test_encode_decode_no_word_start() {
        let entry = ParentEntryV3 {
            raw_ordinal: 100,
            sti: 5,
            own_len: 12,
            sep_len: 0,
            overlap_len: 0,
            overlap: [0; MAX_OVERLAP_BYTES],
            is_word_start: false,
        };
        let val = encode_single_parent_v3(&entry);
        match decode_output_v3(val) {
            ParentRefV3::Single(p) => {
                assert_eq!(p, entry);
                assert!(!p.is_word_start);
            }
            _ => panic!("expected single"),
        }
    }

    #[test]
    fn test_encode_decode_max_values() {
        let entry = ParentEntryV3 {
            raw_ordinal: ORDINAL_MASK,
            sti: STI_MASK as u16,
            own_len: OWN_LEN_MASK as u16,
            sep_len: SEP_LEN_MASK as u8,
            overlap_len: OVERLAP_MASK as u8,
            overlap: [0; MAX_OVERLAP_BYTES],
            is_word_start: true,
        };
        let val = encode_single_parent_v3(&entry);
        match decode_output_v3(val) {
            ParentRefV3::Single(p) => assert_eq!(p, entry),
            _ => panic!("expected single"),
        }
    }

    #[test]
    fn test_encode_decode_multi() {
        let val = encode_multi_parent_v3(9999);
        match decode_output_v3(val) {
            ParentRefV3::Multi { offset } => assert_eq!(offset, 9999),
            _ => panic!("expected multi"),
        }
    }

    /// A version-8 record is flat up to `FLAT_RECORD_MAX_PARENTS` parents
    /// and grouped by overlap beyond, the groups a walk does not need being
    /// skipped unread; both decode to the same parents, `own_len` coming
    /// from the key unless the record spells it out.
    #[test]
    fn v8_record_flat_then_grouped_by_overlap() {
        // Key `\x01tex_`: body 4 bytes, so a chunk parent at sti 2 has own_len 6.
        let key = b"\x01tex_";
        let mk = |ord: u64, sti: u16, ov: &[u8]| {
            let mut overlap = [0u8; MAX_OVERLAP_BYTES];
            overlap[..ov.len()].copy_from_slice(ov);
            ParentEntryV3 { raw_ordinal: ord, sti, own_len: 4 + sti, sep_len: 1, overlap_len: ov.len() as u8, overlap, is_word_start: sti == 0 }
        };
        // Small: flat, in ordinal order, each parent with its overlap.
        let parents = vec![mk(5, 0, b"lo"), mk(3, 2, b"co"), mk(9, 0, b""), mk(7, 1, b"lo"), mk(70_000, 0, b"co")];
        let rec = encode_parent_entries_v8(&parents, key);
        assert_eq!(rec[0], 5, "flat header = count");
        let mut by_ord = parents.clone();
        by_ord.sort_by_key(|p| p.raw_ordinal);
        assert_eq!(decode_parent_entries_v8(&rec, key), by_ord);
        let lo = decode_parent_entries_v8_where(&rec, key, |ov| ov == b"lo");
        assert_eq!(lo.iter().map(|p| p.raw_ordinal).collect::<Vec<_>>(), vec![5, 7]);
        let one = encode_parent_entries_v8(&[mk(12, 3, b"lo")], key);
        assert_eq!(one.len(), 1 + 1 + 1 + 1 + 2, "header, Δord, sti, flags, overlap — no own_len, sep_len in the flags");
        assert_eq!(decode_parent_entries_v8(&one, key), vec![mk(12, 3, b"lo")]);

        // A word entry's own_len adds its sep_len; a long sep_len escapes;
        // an own_len the key does not imply is spelled out.
        let wkey = b"\x02mutex";
        let mut w = mk(4, 0, b"lo"); w.sep_len = 9; w.own_len = 5 + 9;
        let mut odd = mk(8, 1, b"");  odd.own_len = 40; // not 5 + 1
        let rec = encode_parent_entries_v8(&[w.clone(), odd.clone()], wkey);
        assert_eq!(decode_parent_entries_v8(&rec, wkey), vec![w, odd]);

        // Large: three groups in overlap order ("", "co", "lo"), skippable.
        let mut many: Vec<ParentEntryV3> = Vec::new();
        for i in 0..40u64 { many.push(mk(1000 + i * 3, 1, b"lo")); }
        for i in 0..20u64 { many.push(mk(50 + i, 2, b"co")); }
        for i in 0..5u64 { many.push(mk(90_000 + i * 7, 0, b"")); }
        let rec = encode_parent_entries_v8(&many, key);
        assert_eq!(rec[0], 0x80 | 3, "grouped header = group count");
        let all = decode_parent_entries_v8(&rec, key);
        assert_eq!(all.len(), 65);
        let mut expected = many.clone();
        expected.sort_by(|a, b| a.overlap.cmp(&b.overlap).then(a.raw_ordinal.cmp(&b.raw_ordinal)));
        assert_eq!(all, expected, "groups in overlap order, ordinals within");
        let lo = decode_parent_entries_v8_where(&rec, key, |ov| ov == b"lo");
        assert_eq!(lo.len(), 40);
        assert_eq!(lo[0].raw_ordinal, 1000);
        assert_eq!(lo[39].raw_ordinal, 1000 + 39 * 3);
        let none = decode_parent_entries_v8_where(&rec, key, |ov| ov == b"");
        assert_eq!(none.iter().map(|p| p.raw_ordinal).collect::<Vec<_>>(), (0..5).map(|i| 90_000 + i * 7).collect::<Vec<_>>());
        assert!(decode_parent_entries_v8_where(&rec, key, |ov| ov == b"xx").is_empty());
    }

    /// Lowercasing may change a byte length; the own bytes and the overlap
    /// are lowercased apart so the key and the record stay consistent.
    #[test]
    fn lowercase_that_changes_length_keeps_key_and_overlap_apart() {
        let mut builder = SuffixFstBuilderV3::with_min_suffix_len(1);
        // "İX" (3 bytes) + overlap "İ" (2 bytes): lowercase "i̇x" is 4 bytes, "i̇" 3.
        builder.add_token("İXİ", 0, 3, 0, 2, true);
        let (fst_bytes, table) = builder.build().unwrap();
        let fst = lucivy_fst::Map::new(fst_bytes).unwrap();
        let val = fst_get(&fst, SI0_PREFIX, "i̇x".as_bytes()).expect("own bytes, lowercased, are the key");
        let p = single(&table, val, SI0_PREFIX, "i̇x".as_bytes());
        assert_eq!(p.own_len, 3, "metadata stays the collector's");
        assert_eq!(&p.overlap[..p.overlap_len as usize], "i̇".as_bytes());
    }

    #[test]
    fn test_output_table_round_trip() {
        let entries = vec![
            ParentEntryV3 {
                raw_ordinal: 5, sti: 0, own_len: 6, sep_len: 1,
                overlap_len: 2, overlap: [0; MAX_OVERLAP_BYTES], is_word_start: true,
            },
            ParentEntryV3 {
                raw_ordinal: 12, sti: 3, own_len: 8, sep_len: 0,
                overlap_len: 2, overlap: [0; MAX_OVERLAP_BYTES], is_word_start: false,
            },
        ];
        let bytes = encode_parent_entries_v3(&entries);
        assert_eq!(bytes.len(), 1 + 2 * 5, "varint count + 5 bytes per parent here");
        let decoded = decode_parent_entries_v3(&bytes);
        // Sorted by (ordinal, sti): [ord 5, ord 12]
        assert_eq!(decoded, entries);
    }

    /// The version-3 record shape, as written before container version 4.
    fn encode_parent_entries_legacy(parents: &[ParentEntryV3]) -> Vec<u8> {
        let mut buf = (parents.len() as u32).to_le_bytes().to_vec();
        for p in parents {
            buf.extend_from_slice(&(p.raw_ordinal as u32).to_le_bytes());
            buf.extend_from_slice(&p.sti.to_le_bytes());
            buf.extend_from_slice(&p.own_len.to_le_bytes());
            buf.push(p.sep_len);
            buf.push(p.overlap_len);
            buf.push(if p.is_word_start { 1 } else { 0 });
        }
        buf
    }

    #[test]
    fn packed_and_legacy_records_decode_to_the_same_parents() {
        let entries: Vec<ParentEntryV3> = (0..300u64).map(|i| ParentEntryV3 {
            raw_ordinal: i * 7919 % ORDINAL_MASK,
            sti: (i % 200) as u16,
            own_len: (i % 300) as u16 + 1,
            sep_len: (i % 5) as u8,
            overlap_len: (i % 3) as u8,
            overlap: [0; MAX_OVERLAP_BYTES],
            is_word_start: i % 2 == 0,
        }).collect();
        let mut by_ord = entries.clone();
        by_ord.sort_by_key(|p| (p.raw_ordinal, p.sti));

        let delta = decode_parent_entries_v3(&encode_parent_entries_v3(&entries));
        let legacy = decode_parent_entries_v3_legacy(&encode_parent_entries_legacy(&by_ord));
        let packed = decode_parent_entries_v4_packed(&encode_parent_entries_packed(&by_ord));
        assert_eq!(delta, by_ord);
        assert_eq!(legacy, by_ord);
        assert_eq!(packed, by_ord);
    }

    /// The version-4 record shape: varint count + packed u64 per parent.
    fn encode_parent_entries_packed(parents: &[ParentEntryV3]) -> Vec<u8> {
        let mut buf = Vec::new();
        super::super::varint::write_varint(&mut buf, parents.len() as u64);
        for p in parents { buf.extend_from_slice(&encode_single_parent_v3(p).to_le_bytes()); }
        buf
    }

    /// Dense lists — the frequent keys — cost about two bytes per parent.
    #[test]
    fn dense_lists_compress_to_two_bytes_per_parent() {
        let entries: Vec<ParentEntryV3> = (0..50_000u64).map(|i| ParentEntryV3 {
            raw_ordinal: i * 22, sti: 5, own_len: 6, sep_len: 1, overlap_len: 2, overlap: [0; MAX_OVERLAP_BYTES], is_word_start: false,
        }).collect();
        let bytes = encode_parent_entries_v3(&entries);
        assert!(bytes.len() <= 3 + 50_000 * 5, "{}", bytes.len());
        assert_eq!(decode_parent_entries_v3(&bytes), entries);
    }

    #[test]
    fn packed_record_holds_the_largest_values() {
        let p = ParentEntryV3 {
            raw_ordinal: ORDINAL_MASK,
            sti: STI_MASK as u16,
            own_len: OWN_LEN_MASK as u16,
            sep_len: SEP_LEN_MASK as u8,
            overlap_len: OVERLAP_MASK as u8,
            overlap: [0; MAX_OVERLAP_BYTES],
            is_word_start: true,
        };
        let big = vec![p.clone(); 70_000]; // count above u16, needs a 3-byte varint
        let bytes = encode_parent_entries_v3(&big);
        // first parent: 4-byte ordinal delta; the rest: delta 0 → 1 byte; sti 4095 and own_len 16383: 2 bytes each
        assert_eq!(bytes.len(), 3 + 4 + 69_999 + 70_000 * (2 + 2 + 1 + 1));
        let decoded = decode_parent_entries_v3(&bytes);
        assert_eq!(decoded.len(), 70_000);
        assert_eq!(decoded[69_999], p);
    }

    /// No key starts inside the overlap: for `mutex_` + `lo`, the suffixes
    /// `lo` and `o` pointing at `mutex_` are the next token's business.
    #[test]
    fn no_suffix_starts_in_the_overlap() {
        let mut builder = SuffixFstBuilderV3::with_min_suffix_len(1);
        builder.add_token("mutex_lo", 0, 6, 1, 2, true);
        builder.add_token("lock", 1, 4, 0, 0, true);
        let with_overlap: Vec<(u16, u64)> = builder.entries.iter()
            .map(|(_, _, p)| (p.sti, p.raw_ordinal)).collect();
        assert!(with_overlap.iter().all(|&(sti, ord)| sti < if ord == 0 { 6 } else { 4 }),
            "a suffix starts in the overlap: {with_overlap:?}");
        // mutex_lo: 6 suffixes, cut at the boundary, no marker; lock: 4.
        assert_eq!(builder.entries.len(), 6 + 4);
        let (fst_bytes, table) = builder.build().unwrap();
        let file = crate::suffix_fst::file_v3::SfxFileWriterV3::new(fst_bytes, table).to_bytes();
        let reader = crate::suffix_fst::file_v3::SfxFileReaderV3::open(&file).unwrap();
        // The exact keys "lo" and "o" pointing at `mutex_` are gone; the same
        // text positions are `lock`'s own suffixes at sti 0 and 1.
        assert!(reader.resolve_suffix("lo").is_empty());
        assert!(reader.resolve_suffix("o").is_empty());
        let lock: Vec<(u64, u16)> = reader.resolve_suffix("lock").iter().map(|p| (p.raw_ordinal, p.sti)).collect();
        assert_eq!(lock, vec![(1, 0)]);
        let ock: Vec<(u64, u16)> = reader.resolve_suffix("ock").iter().map(|p| (p.raw_ordinal, p.sti)).collect();
        assert_eq!(ock, vec![(1, 1)]);
        // The cross-token trigram still resolves: key `x_`, overlap `lo` in the record.
        assert!(reader.resolve_suffix("x_").iter().any(|p| p.raw_ordinal == 0 && p.sti == 4 && &p.overlap[..2] == b"lo"));
    }

    // ── Builder with overlap ──

    /// The one parent behind a version-6 FST value.
    /// The one parent behind a key's value; the key is needed since
    /// version 8 (`own_len` comes from it).
    fn single(table: &[u8], val: u64, prefix: u8, body: &[u8]) -> ParentEntryV3 {
        let mut key = vec![prefix];
        key.extend_from_slice(body);
        let mut v = decode_parent_entries_v8(lucivy_fst::OutputTable::new(table).get(val), &key);
        assert_eq!(v.len(), 1, "expected single");
        v.pop().unwrap()
    }

    fn fst_get(fst: &lucivy_fst::Map<Vec<u8>>, prefix: u8, key: &[u8]) -> Option<u64> {
        let mut prefixed = vec![prefix];
        prefixed.extend_from_slice(key);
        fst.get(&prefixed)
    }

    #[test]
    fn test_builder_v3_basic() {
        let mut builder = SuffixFstBuilderV3::with_min_suffix_len(1);
        // Token "mutex_" (own_len=6, sep=1) + overlap "lo" → extended "mutex_lo"
        builder.add_token("mutex_lo", 0, 6, 1, 2, true);

        let (fst_bytes, table) = builder.build().unwrap();
        let fst = lucivy_fst::Map::new(fst_bytes).unwrap();

        // SI=0: the key is the token's own bytes, the overlap is in the record.
        assert!(fst_get(&fst, SI0_PREFIX, b"mutex_lo").is_none(), "no overlap in the key");
        let val = fst_get(&fst, SI0_PREFIX, b"mutex_").expect("mutex_ at SI=0");
        {
            let p = single(&table, val, SI0_PREFIX, b"mutex_");
            assert_eq!(&p.overlap[..2], b"lo");
            assert_eq!(p.raw_ordinal, 0);
            assert_eq!(p.sti, 0);
            assert_eq!(p.own_len, 6);
            assert_eq!(p.sep_len, 1);
            assert_eq!(p.overlap_len, 2);
            assert!(p.is_word_start);
            assert_eq!(p.content_len(), 5);
        }

        // SI=4 "x_" — the cross-boundary trigram "x_l" is this key plus its overlap
        let val = fst_get(&fst, SI_REST_PREFIX, b"x_").expect("x_ at SI>0");
        {
            let p = single(&table, val, SI_REST_PREFIX, b"x_");
            assert_eq!(p.sti, 4);
            assert_eq!(p.own_len, 6);
            assert_eq!(&p.overlap[..2], b"lo");
        }

        // SI=6 "lo" would start in the overlap zone: not a key since 4 September 2026.
        assert!(fst_get(&fst, SI_REST_PREFIX, b"lo").is_none(), "no suffix starts in the overlap");
        assert!(fst_get(&fst, SI_REST_PREFIX, b"o").is_none());
    }

    #[test]
    fn test_builder_v3_multi_parent_overlap() {
        let mut builder = SuffixFstBuilderV3::with_min_suffix_len(1);
        // "mutex_lo" ord=0 has suffix "lo" at SI=6 (overlap zone)
        builder.add_token("mutex_lo", 0, 6, 1, 2, true);
        // "login_" ord=1: suffix "lo" at SI=0 in SI0 partition
        builder.add_token("login_", 1, 6, 1, 0, true);

        let (fst_bytes, table) = builder.build().unwrap();
        let fst = lucivy_fst::Map::new(fst_bytes).unwrap();

        // "lo" in SI>0 would be mutex_lo's overlap zone (sti 6 >= own_len 6):
        // not a key any more; `login_` carries it at SI=0.
        assert!(fst_get(&fst, SI_REST_PREFIX, b"lo").is_none(), "no suffix starts in the overlap");

        // "login_" in SI=0 partition
        let val = fst_get(&fst, SI0_PREFIX, b"login_").expect("login_ at SI=0");
        {
            let p = single(&table, val, SI0_PREFIX, b"login_");
            assert_eq!(p.raw_ordinal, 1);
            assert_eq!(p.sti, 0);
            assert!(p.is_word_start);
        }

        // "ogin_" in SI>0 partition: multi-parent? No — only from "login_"
        let val = fst_get(&fst, SI_REST_PREFIX, b"ogin_").expect("ogin_ in SI>0");
        {
            let p = single(&table, val, SI_REST_PREFIX, b"ogin_");
            assert_eq!(p.raw_ordinal, 1);
            assert_eq!(p.sti, 1);
        }
    }

    #[test]
    fn test_builder_v3_no_overlap_last_token() {
        let mut builder = SuffixFstBuilderV3::with_min_suffix_len(1);
        // Last token: no overlap
        builder.add_token("init", 2, 4, 0, 0, true);

        let (fst_bytes, table) = builder.build().unwrap();
        let fst = lucivy_fst::Map::new(fst_bytes).unwrap();

        let val = fst_get(&fst, SI0_PREFIX, b"init").expect("init at SI=0");
        {
            let p = single(&table, val, SI0_PREFIX, b"init");
            assert_eq!(p.own_len, 4);
            assert_eq!(p.overlap_len, 0);
            assert_eq!(p.sep_len, 0);
        }
    }

    #[test]
    fn test_builder_v3_word_start_propagated() {
        let mut builder = SuffixFstBuilderV3::with_min_suffix_len(1);
        // TI=0 "getEleme" — first chunk, is_word_start=true (lowercased internally)
        builder.add_token("getEleme", 0, 8, 0, 0, true);
        // TI=1 "ntById" — second chunk, is_word_start=false
        builder.add_token("ntById", 1, 6, 0, 0, false);

        let (fst_bytes, table) = builder.build().unwrap();
        let fst = lucivy_fst::Map::new(fst_bytes).unwrap();

        let val = fst_get(&fst, SI0_PREFIX, b"geteleme").expect("getEleme lowered");
        {
            let p = single(&table, val, SI0_PREFIX, b"geteleme");
            assert!(p.is_word_start);
        }

        let val = fst_get(&fst, SI0_PREFIX, b"ntbyid").expect("ntById lowered");
        {
            let p = single(&table, val, SI0_PREFIX, b"ntbyid");
            assert!(!p.is_word_start);
        }
    }

    #[test]
    fn test_content_len_derived() {
        let p = ParentEntryV3 {
            raw_ordinal: 0, sti: 0, own_len: 8, sep_len: 1,
            overlap_len: 2, overlap: [0; MAX_OVERLAP_BYTES], is_word_start: true,
        };
        assert_eq!(p.content_len(), 7); // 8 - 1
    }

    // ── Sep-stripped partition (0x02) ──

    #[test]
    fn test_stripped_partition_exists() {
        let mut builder = SuffixFstBuilderV3::with_min_suffix_len(1);
        // "mutex_lo" : content=5 ("mutex"), sep=1 ("_"), overlap=2 ("lo")
        builder.add_token("mutex_lo", 0, 6, 1, 2, true);
        builder.add_word_stripped("mutex", "lo", 0, 6, 1, true);

        let (fst_bytes, table) = builder.build().unwrap();
        let fst = lucivy_fst::Map::new(fst_bytes).unwrap();

        // Partition 0x02 holds the word's content suffixes; the content
        // overlap "lo" is in the record. "mutex" at STI=0
        assert!(fst_get(&fst, SI_STRIPPED_PREFIX, b"mutexlo").is_none());
        let val = fst_get(&fst, SI_STRIPPED_PREFIX, b"mutex").expect("mutex in stripped");
        {
            let p = single(&table, val, SI_STRIPPED_PREFIX, b"mutex");
            assert_eq!(&p.overlap[..2], b"lo");
            assert_eq!(p.sti, 0);
            assert_eq!(p.raw_ordinal, 0);
            assert_eq!(p.own_len, 6);
            assert_eq!(p.sep_len, 1);
        }

        // "ex" at STI=3 — the trigram "exl" is this key plus its overlap
        let val = fst_get(&fst, SI_STRIPPED_PREFIX, b"ex").expect("ex in stripped");
        {
            let p = single(&table, val, SI_STRIPPED_PREFIX, b"ex");
            assert_eq!(p.sti, 3);
        }
    }

    #[test]
    fn test_stripped_trigram_cross_sep() {
        let mut builder = SuffixFstBuilderV3::with_min_suffix_len(1);
        builder.add_token("mutex_lo", 0, 6, 1, 2, true);
        builder.add_word_stripped("mutex", "lo", 0, 6, 1, true);

        let (fst_bytes, table) = builder.build().unwrap();
        let fst = lucivy_fst::Map::new(fst_bytes).unwrap();

        // "exl" trigram: NOT in partition 0x01 (normal has "ex_lo", not "exlo")
        assert!(fst_get(&fst, SI_REST_PREFIX, b"exl").is_none(),
            "exl should NOT be in normal partition (has ex_lo not exlo)");

        // "exl" IS a prefix of "exlo" in partition 0x02
        // Check via range scan
        use lucivy_fst::{IntoStreamer, Streamer};
        let ge = [SI_STRIPPED_PREFIX, b'e', b'x', b'l'];
        let lt = [SI_STRIPPED_PREFIX, b'e', b'x', b'm']; // 'm' > 'l'
        let mut stream = fst.range().ge(&ge[..]).lt(&lt[..]).into_stream();
        let mut found = false;
        while let Some((key, _)) = stream.next() {
            if key.len() > 1 && key[1..].starts_with(b"exl") {
                found = true;
                break;
            }
        }
        // The key is `ex` and the record says `lo` follows: what
        // `fst_candidates_v3` probes for; the range scan alone no longer sees it.
        assert!(!found, "keys stop at the word's content; the overlap is in the record");
        let key = [SI_STRIPPED_PREFIX, b'e', b'x'];
        let val = fst.get(key).expect("ex in stripped");
        let p = single(&table, val, SI_STRIPPED_PREFIX, b"ex");
        assert_eq!((p.sti, &p.overlap[..2]), (3, &b"lo"[..]));
    }

    #[test]
    fn test_no_stripped_when_no_sep() {
        let mut builder = SuffixFstBuilderV3::with_min_suffix_len(1);
        // Token without sep — no stripped entries should be added
        builder.add_token("lock", 0, 4, 0, 0, true);

        let (fst_bytes, table) = builder.build().unwrap();
        let fst = lucivy_fst::Map::new(fst_bytes).unwrap();

        // No entries in partition 0x02
        assert!(fst_get(&fst, SI_STRIPPED_PREFIX, b"lock").is_none());
        assert!(fst_get(&fst, SI_STRIPPED_PREFIX, b"ock").is_none());

        // But normal partitions work
        assert!(fst_get(&fst, SI0_PREFIX, b"lock").is_some());
        assert!(fst_get(&fst, SI_REST_PREFIX, b"ock").is_some());
    }

    #[test]
    fn test_stripped_long_sep() {
        let mut builder = SuffixFstBuilderV3::with_min_suffix_len(1);
        // "a____bc" : content=1 ("a"), sep=4 ("____"), overlap=2 ("bc")
        builder.add_token("a____bc", 0, 5, 4, 2, true);
        builder.add_word_stripped("a", "bc", 0, 5, 4, true);

        let (fst_bytes, table) = builder.build().unwrap();
        let fst = lucivy_fst::Map::new(fst_bytes).unwrap();

        // Normal partition has "a____" with the underscores, "bc" in the record
        assert!(fst_get(&fst, SI0_PREFIX, b"a____").is_some());

        // Stripped partition has "a" (content only; overlap "bc" in the record)
        let val = fst_get(&fst, SI_STRIPPED_PREFIX, b"a").expect("a in stripped");
        {
            let p = single(&table, val, SI_STRIPPED_PREFIX, b"a");
            assert_eq!(p.sti, 0);
            assert_eq!(p.own_len, 5); // a(1) + ____(4)
            assert_eq!(p.sep_len, 4);
            assert_eq!(p.overlap_len, 2);
        }
    }

    #[test]
    fn test_stripped_preserves_ordinal() {
        let mut builder = SuffixFstBuilderV3::with_min_suffix_len(1);
        builder.add_token("mutex_lo", 42, 6, 1, 2, true);
        builder.add_word_stripped("mutex", "lo", 42, 6, 1, true);

        let (fst_bytes, table) = builder.build().unwrap();
        let fst = lucivy_fst::Map::new(fst_bytes).unwrap();

        // Normal and stripped entries should have the same ordinal
        let normal = fst_get(&fst, SI0_PREFIX, b"mutex_").unwrap();
        let stripped = fst_get(&fst, SI_STRIPPED_PREFIX, b"mutex").unwrap();

        let (n, s) = (single(&table, normal, SI0_PREFIX, b"mutex_"), single(&table, stripped, SI_STRIPPED_PREFIX, b"mutex"));
        assert_eq!(n.raw_ordinal, s.raw_ordinal, "same ordinal");
        assert_eq!(n.raw_ordinal, 42);
    }
}
