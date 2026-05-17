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

/// A parent entry with v3 metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParentEntryV3 {
    pub raw_ordinal: u64,
    pub sti: u16,
    pub own_len: u16,
    pub sep_len: u8,
    pub overlap_len: u8,
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
    Single(ParentEntryV3),
    Multi { offset: u64 },
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
            is_word_start: value & WORD_START_FLAG != 0,
        })
    }
}

/// Encode v3 parent entries into bytes for the OutputTable.
/// Format per entry: [u32 ordinal][u16 sti][u16 own_len][u8 sep_len][u8 overlap_len][u8 flags] = 11 bytes.
pub fn encode_parent_entries_v3(parents: &[ParentEntryV3]) -> Vec<u8> {
    let mut sorted = parents.to_vec();
    sorted.sort_by_key(|p| p.sti);
    let mut buf = Vec::with_capacity(2 + sorted.len() * 11);
    buf.extend_from_slice(&(sorted.len() as u16).to_le_bytes());
    for p in &sorted {
        buf.extend_from_slice(&(p.raw_ordinal as u32).to_le_bytes());
        buf.extend_from_slice(&p.sti.to_le_bytes());
        buf.extend_from_slice(&p.own_len.to_le_bytes());
        buf.push(p.sep_len);
        buf.push(p.overlap_len);
        buf.push(if p.is_word_start { 1 } else { 0 });
    }
    buf
}

/// Decode v3 parent entries from OutputTable bytes.
pub fn decode_parent_entries_v3(data: &[u8]) -> Vec<ParentEntryV3> {
    let num = u16::from_le_bytes([data[0], data[1]]) as usize;
    let mut cursor = 2;
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
            raw_ordinal, sti, own_len, sep_len, overlap_len, is_word_start,
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
}

impl Default for SuffixFstBuilderV3 {
    fn default() -> Self {
        Self::new()
    }
}

impl SuffixFstBuilderV3 {
    pub fn new() -> Self {
        Self::with_min_suffix_len(default_min_suffix_len())
    }

    pub fn with_min_suffix_len(min: usize) -> Self {
        Self {
            key_buf: Vec::new(),
            entries: Vec::new(),
            min_suffix_len: min,
            num_terms: 0,
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
        let lower = extended_token.to_lowercase();
        let extended_bytes = lower.as_bytes();
        let extended_len = extended_bytes.len();
        let max_si = extended_len.min(MAX_CHUNK_BYTES);

        let content_len = own_len as usize - sep_len as usize;

        // ── Normal suffixes (partitions 0x00 and 0x01) ──
        for si in 0..max_si {
            if si > 0 && !is_utf8_char_boundary(extended_bytes, si) {
                continue;
            }
            let suffix = &extended_bytes[si..];
            if si > 0 && suffix.len() < self.min_suffix_len {
                break;
            }

            let prefix = if si == 0 { SI0_PREFIX } else { SI_REST_PREFIX };
            let key_start = self.key_buf.len() as u32;
            self.key_buf.push(prefix);
            self.key_buf.extend_from_slice(suffix);
            let key_len = (self.key_buf.len() as u32) - key_start;

            self.entries.push((
                key_start,
                key_len,
                ParentEntryV3 {
                    raw_ordinal,
                    sti: si as u16,
                    own_len,
                    sep_len,
                    overlap_len,
                    is_word_start,
                },
            ));
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
        first_own_len: u16,
        first_sep_len: u8,
        is_word_start: bool,
    ) {
        let lower_content = word_content.to_lowercase();
        let lower_overlap = content_overlap.to_lowercase();
        let content_bytes = lower_content.as_bytes();
        let overlap_bytes = lower_overlap.as_bytes();
        let content_len = content_bytes.len();

        if content_len == 0 {
            return;
        }

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

            let key_start = self.key_buf.len() as u32;
            self.key_buf.push(SI_STRIPPED_PREFIX);
            self.key_buf.extend_from_slice(suffix_content);
            self.key_buf.extend_from_slice(overlap_bytes);
            let key_len = (self.key_buf.len() as u32) - key_start;

            self.entries.push((
                key_start,
                key_len,
                ParentEntryV3 {
                    raw_ordinal: first_ordinal,
                    sti: si as u16,
                    own_len: (content_len + first_sep_len as usize) as u16, // content + sep, like normal tokens
                    sep_len: first_sep_len,
                    overlap_len: overlap_bytes.len() as u8,
                    is_word_start,
                },
            ));
        }
    }

    /// Build the FST and output table bytes.
    pub fn build(mut self) -> Result<(Vec<u8>, Vec<u8>), lucivy_fst::Error> {
        let buf = &self.key_buf;
        self.entries.sort_by(|a, b| {
            buf[a.0 as usize..(a.0 + a.1) as usize]
                .cmp(&buf[b.0 as usize..(b.0 + b.1) as usize])
                .then(a.2.raw_ordinal.cmp(&b.2.raw_ordinal))
                .then(a.2.sti.cmp(&b.2.sti))
        });
        self.entries.dedup_by(|a, b| {
            buf[a.0 as usize..(a.0 + a.1) as usize] == buf[b.0 as usize..(b.0 + b.1) as usize]
                && a.2.raw_ordinal == b.2.raw_ordinal
                && a.2.sti == b.2.sti
        });

        let mut fst_builder = MapBuilder::memory();
        let mut output_table = OutputTableBuilder::new();
        self.num_terms = 0;

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

            let output = if num_parents == 1 {
                encode_single_parent_v3(&self.entries[i].2)
            } else {
                let parents: Vec<ParentEntryV3> = self.entries[i..j]
                    .iter()
                    .map(|e| e.2.clone())
                    .collect();
                let record = encode_parent_entries_v3(&parents);
                let offset = output_table.add(&record);
                encode_multi_parent_v3(offset)
            };
            fst_builder.insert(key, output)?;
            self.num_terms += 1;

            i = j;
        }

        let fst_bytes = fst_builder.into_inner()?;
        Ok((fst_bytes, output_table.into_inner()))
    }

    pub fn num_terms(&self) -> usize {
        self.num_terms
    }
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

    #[test]
    fn test_output_table_round_trip() {
        let entries = vec![
            ParentEntryV3 {
                raw_ordinal: 5, sti: 0, own_len: 6, sep_len: 1,
                overlap_len: 2, is_word_start: true,
            },
            ParentEntryV3 {
                raw_ordinal: 12, sti: 3, own_len: 8, sep_len: 0,
                overlap_len: 2, is_word_start: false,
            },
        ];
        let bytes = encode_parent_entries_v3(&entries);
        let decoded = decode_parent_entries_v3(&bytes);
        // Sorted by sti, so order should be [sti=0, sti=3]
        assert_eq!(decoded[0].raw_ordinal, 5);
        assert_eq!(decoded[1].raw_ordinal, 12);
    }

    // ── Builder with overlap ──

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

        let (fst_bytes, _) = builder.build().unwrap();
        let fst = lucivy_fst::Map::new(fst_bytes).unwrap();

        // SI=0 "mutex_lo"
        let val = fst_get(&fst, SI0_PREFIX, b"mutex_lo").expect("mutex_lo at SI=0");
        match decode_output_v3(val) {
            ParentRefV3::Single(p) => {
                assert_eq!(p.raw_ordinal, 0);
                assert_eq!(p.sti, 0);
                assert_eq!(p.own_len, 6);
                assert_eq!(p.sep_len, 1);
                assert_eq!(p.overlap_len, 2);
                assert!(p.is_word_start);
                assert_eq!(p.content_len(), 5);
            }
            _ => panic!("expected single"),
        }

        // SI=4 "x_lo" — the cross-boundary trigram "x_l" is in here
        let val = fst_get(&fst, SI_REST_PREFIX, b"x_lo").expect("x_lo at SI>0");
        match decode_output_v3(val) {
            ParentRefV3::Single(p) => {
                assert_eq!(p.sti, 4);
                assert_eq!(p.own_len, 6);
            }
            _ => panic!("expected single"),
        }

        // SI=6 "lo" — overlap zone
        let val = fst_get(&fst, SI_REST_PREFIX, b"lo").expect("lo at SI>0");
        match decode_output_v3(val) {
            ParentRefV3::Single(p) => {
                assert_eq!(p.sti, 6);
                // sti(6) >= own_len(6) → overlap zone
            }
            _ => panic!("expected single"),
        }
    }

    #[test]
    fn test_builder_v3_multi_parent_overlap() {
        let mut builder = SuffixFstBuilderV3::with_min_suffix_len(1);
        // "mutex_lo" ord=0 has suffix "lo" at SI=6 (overlap zone)
        builder.add_token("mutex_lo", 0, 6, 1, 2, true);
        // "login_" ord=1: suffix "lo" at SI=0 in SI0 partition
        builder.add_token("login_", 1, 6, 1, 0, true);

        let (fst_bytes, output_table_data) = builder.build().unwrap();
        let fst = lucivy_fst::Map::new(fst_bytes).unwrap();

        // "lo" in SI>0 partition: single parent (mutex_lo, sti=6, overlap zone)
        let val = fst_get(&fst, SI_REST_PREFIX, b"lo").expect("lo in SI>0");
        match decode_output_v3(val) {
            ParentRefV3::Single(p) => {
                assert_eq!(p.raw_ordinal, 0); // mutex_lo
                assert_eq!(p.sti, 6);
            }
            _ => panic!("expected single"),
        }

        // "login_" in SI=0 partition
        let val = fst_get(&fst, SI0_PREFIX, b"login_").expect("login_ at SI=0");
        match decode_output_v3(val) {
            ParentRefV3::Single(p) => {
                assert_eq!(p.raw_ordinal, 1);
                assert_eq!(p.sti, 0);
                assert!(p.is_word_start);
            }
            _ => panic!("expected single"),
        }

        // "ogin_" in SI>0 partition: multi-parent? No — only from "login_"
        let val = fst_get(&fst, SI_REST_PREFIX, b"ogin_").expect("ogin_ in SI>0");
        match decode_output_v3(val) {
            ParentRefV3::Single(p) => {
                assert_eq!(p.raw_ordinal, 1);
                assert_eq!(p.sti, 1);
            }
            _ => panic!("expected single"),
        }
    }

    #[test]
    fn test_builder_v3_no_overlap_last_token() {
        let mut builder = SuffixFstBuilderV3::with_min_suffix_len(1);
        // Last token: no overlap
        builder.add_token("init", 2, 4, 0, 0, true);

        let (fst_bytes, _) = builder.build().unwrap();
        let fst = lucivy_fst::Map::new(fst_bytes).unwrap();

        let val = fst_get(&fst, SI0_PREFIX, b"init").expect("init at SI=0");
        match decode_output_v3(val) {
            ParentRefV3::Single(p) => {
                assert_eq!(p.own_len, 4);
                assert_eq!(p.overlap_len, 0);
                assert_eq!(p.sep_len, 0);
            }
            _ => panic!("expected single"),
        }
    }

    #[test]
    fn test_builder_v3_word_start_propagated() {
        let mut builder = SuffixFstBuilderV3::with_min_suffix_len(1);
        // TI=0 "getEleme" — first chunk, is_word_start=true (lowercased internally)
        builder.add_token("getEleme", 0, 8, 0, 0, true);
        // TI=1 "ntById" — second chunk, is_word_start=false
        builder.add_token("ntById", 1, 6, 0, 0, false);

        let (fst_bytes, _) = builder.build().unwrap();
        let fst = lucivy_fst::Map::new(fst_bytes).unwrap();

        let val = fst_get(&fst, SI0_PREFIX, b"geteleme").expect("getEleme lowered");
        match decode_output_v3(val) {
            ParentRefV3::Single(p) => assert!(p.is_word_start),
            _ => panic!("expected single"),
        }

        let val = fst_get(&fst, SI0_PREFIX, b"ntbyid").expect("ntById lowered");
        match decode_output_v3(val) {
            ParentRefV3::Single(p) => assert!(!p.is_word_start),
            _ => panic!("expected single"),
        }
    }

    #[test]
    fn test_content_len_derived() {
        let p = ParentEntryV3 {
            raw_ordinal: 0, sti: 0, own_len: 8, sep_len: 1,
            overlap_len: 2, is_word_start: true,
        };
        assert_eq!(p.content_len(), 7); // 8 - 1
    }

    // ── Sep-stripped partition (0x02) ──

    #[test]
    fn test_stripped_partition_exists() {
        let mut builder = SuffixFstBuilderV3::with_min_suffix_len(1);
        // "mutex_lo" : content=5 ("mutex"), sep=1 ("_"), overlap=2 ("lo")
        builder.add_token("mutex_lo", 0, 6, 1, 2, true);

        let (fst_bytes, _) = builder.build().unwrap();
        let fst = lucivy_fst::Map::new(fst_bytes).unwrap();

        // Partition 0x02 should have stripped suffixes (content + overlap, no sep)
        // "mutexlo" at STI=0
        let val = fst_get(&fst, SI_STRIPPED_PREFIX, b"mutexlo").expect("mutexlo in stripped");
        match decode_output_v3(val) {
            ParentRefV3::Single(p) => {
                assert_eq!(p.sti, 0);
                assert_eq!(p.raw_ordinal, 0);
                assert_eq!(p.own_len, 6);
                assert_eq!(p.sep_len, 1);
            }
            _ => panic!("expected single"),
        }

        // "exlo" at STI=3 — the trigram "exl" is findable here!
        let val = fst_get(&fst, SI_STRIPPED_PREFIX, b"exlo").expect("exlo in stripped");
        match decode_output_v3(val) {
            ParentRefV3::Single(p) => {
                assert_eq!(p.sti, 3);
            }
            _ => panic!("expected single"),
        }
    }

    #[test]
    fn test_stripped_trigram_cross_sep() {
        let mut builder = SuffixFstBuilderV3::with_min_suffix_len(1);
        builder.add_token("mutex_lo", 0, 6, 1, 2, true);

        let (fst_bytes, _) = builder.build().unwrap();
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
        assert!(found, "exl should be findable as prefix of exlo in stripped partition");
    }

    #[test]
    fn test_no_stripped_when_no_sep() {
        let mut builder = SuffixFstBuilderV3::with_min_suffix_len(1);
        // Token without sep — no stripped entries should be added
        builder.add_token("lock", 0, 4, 0, 0, true);

        let (fst_bytes, _) = builder.build().unwrap();
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

        let (fst_bytes, _) = builder.build().unwrap();
        let fst = lucivy_fst::Map::new(fst_bytes).unwrap();

        // Normal partition has "a____bc" with the underscores
        assert!(fst_get(&fst, SI0_PREFIX, b"a____bc").is_some());

        // Stripped partition has "abc" (content "a" + overlap "bc", sep "____" removed)
        let val = fst_get(&fst, SI_STRIPPED_PREFIX, b"abc").expect("abc in stripped");
        match decode_output_v3(val) {
            ParentRefV3::Single(p) => {
                assert_eq!(p.sti, 0);
                assert_eq!(p.own_len, 5); // a(1) + ____(4)
                assert_eq!(p.sep_len, 4);
                assert_eq!(p.overlap_len, 2);
            }
            _ => panic!("expected single"),
        }
    }

    #[test]
    fn test_stripped_preserves_ordinal() {
        let mut builder = SuffixFstBuilderV3::with_min_suffix_len(1);
        builder.add_token("mutex_lo", 42, 6, 1, 2, true);

        let (fst_bytes, _) = builder.build().unwrap();
        let fst = lucivy_fst::Map::new(fst_bytes).unwrap();

        // Normal and stripped entries should have the same ordinal
        let normal = fst_get(&fst, SI0_PREFIX, b"mutex_lo").unwrap();
        let stripped = fst_get(&fst, SI_STRIPPED_PREFIX, b"mutexlo").unwrap();

        match (decode_output_v3(normal), decode_output_v3(stripped)) {
            (ParentRefV3::Single(n), ParentRefV3::Single(s)) => {
                assert_eq!(n.raw_ordinal, s.raw_ordinal, "same ordinal");
                assert_eq!(n.raw_ordinal, 42);
            }
            _ => panic!("expected singles"),
        }
    }
}
