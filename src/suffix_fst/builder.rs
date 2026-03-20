use lucivy_fst::{MapBuilder, OutputTableBuilder};

/// Minimum suffix length to index.
/// Default 1 = index all suffixes (needed for correct substring search).
/// Override via LUCIVY_MIN_SUFFIX_LEN env var for benchmarking.
fn default_min_suffix_len() -> usize {
    std::env::var("LUCIVY_MIN_SUFFIX_LEN")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1)
}

// Encoding layout for single-parent u64 output:
//   bit 63 = 0 (single parent)
//   bits 0-23  = raw_ordinal (up to ~16M tokens)
//   bits 24-39 = SI (16 bits, up to 65535 chars)
//
// For multi-parent:
//   bit 63 = 1
//   bits 0-31  = offset into OutputTable

/// Max suffix depth in bytes. Safety net for tokens not split by the tokenizer.
const MAX_CHUNK_BYTES: usize = 256;

/// Prefix byte for SI=0 entries (full token start) in the suffix FST.
/// startsWith queries range-scan only these entries.
pub const SI0_PREFIX: u8 = 0x00;

/// Prefix byte for SI>0 entries (substring) in the suffix FST.
pub const SI_REST_PREFIX: u8 = 0x01;

const MULTI_PARENT_FLAG: u64 = 1 << 63;
const RAW_ORDINAL_MASK: u64 = 0x00FF_FFFF; // 24 bits
const SI_SHIFT: u32 = 24;
const SI_MASK: u64 = 0xFFFF; // 16 bits

/// Encode a single-parent value into u64.
pub fn encode_single_parent(raw_ordinal: u64, si: u16) -> u64 {
    debug_assert!(raw_ordinal <= RAW_ORDINAL_MASK, "raw_ordinal overflow: {raw_ordinal}");
    debug_assert!((si as u64) <= SI_MASK, "SI overflow: {si}");
    (raw_ordinal & RAW_ORDINAL_MASK) | ((si as u64) << SI_SHIFT)
}

/// Encode a multi-parent offset into u64.
pub fn encode_multi_parent(offset: u64) -> u64 {
    MULTI_PARENT_FLAG | offset
}

/// Decode a u64 FST output value.
pub fn decode_output(value: u64) -> ParentRef {
    if value & MULTI_PARENT_FLAG != 0 {
        ParentRef::Multi {
            offset: value & !MULTI_PARENT_FLAG,
        }
    } else {
        ParentRef::Single {
            raw_ordinal: value & RAW_ORDINAL_MASK,
            si: ((value >> SI_SHIFT) & SI_MASK) as u16,
        }
    }
}

/// Decoded parent reference from an FST output value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParentRef {
    Single { raw_ordinal: u64, si: u16 },
    Multi { offset: u64 },
}

/// A parent entry: which raw token this suffix comes from, and at what offset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParentEntry {
    pub raw_ordinal: u64,
    pub si: u16,
}

/// Encode a list of parent entries into bytes for the OutputTable.
pub fn encode_parent_entries(parents: &[ParentEntry]) -> Vec<u8> {
    let mut sorted = parents.to_vec();
    sorted.sort_by_key(|p| p.si); // SI=0 first → early exit for exact/prefix lookups
    let mut buf = Vec::with_capacity(2 + sorted.len() * 6);
    buf.extend_from_slice(&(sorted.len() as u16).to_le_bytes());
    for p in &sorted {
        buf.extend_from_slice(&(p.raw_ordinal as u32).to_le_bytes());
        buf.extend_from_slice(&p.si.to_le_bytes());
    }
    buf
}

/// Decode parent entries from bytes read from the OutputTable.
pub fn decode_parent_entries(data: &[u8]) -> Vec<ParentEntry> {
    let num_parents = u16::from_le_bytes([data[0], data[1]]) as usize;
    let mut cursor = 2;
    let mut entries = Vec::with_capacity(num_parents);
    for _ in 0..num_parents {
        let raw_ordinal = u32::from_le_bytes([
            data[cursor],
            data[cursor + 1],
            data[cursor + 2],
            data[cursor + 3],
        ]) as u64;
        cursor += 4;
        let si = u16::from_le_bytes([data[cursor], data[cursor + 1]]);
        cursor += 2;
        entries.push(ParentEntry { raw_ordinal, si });
    }
    entries
}

/// Builds a suffix FST from unique tokens.
///
/// Uses batch accumulation: suffix entries are pushed into a Vec during add_token
/// (O(1) per entry), then sorted and grouped at build time (one O(n log n) pass).
/// This is faster than the BTreeMap approach which does O(log n) per insert.
///
/// At build time, produces:
/// - FST bytes: suffix term -> u64 output (encoded parent ref)
/// - OutputTable bytes: for multi-parent suffixes, variable-length records
///   containing packed (raw_ordinal, SI) entries
pub struct SuffixFstBuilder {
    /// Unsorted, with potential duplicates. Sorted and grouped at build time.
    entries: Vec<(String, ParentEntry)>,
    min_suffix_len: usize,
    num_terms: usize,
}

impl SuffixFstBuilder {
    pub fn new() -> Self {
        Self::with_min_suffix_len(default_min_suffix_len())
    }

    pub fn with_min_suffix_len(min: usize) -> Self {
        Self {
            entries: Vec::new(),
            min_suffix_len: min,
            num_terms: 0,
        }
    }

    /// Register all suffixes of a token. Called once per unique token in the segment.
    /// `raw_ordinal` is the term ordinal (sorted alphabetical position).
    ///
    /// Each suffix is prefixed with `SI0_PREFIX` (0x00) for SI=0 entries
    /// or `SI_REST_PREFIX` (0x01) for SI>0. This partitions the FST so that
    /// startsWith queries can range-scan only SI=0 entries.
    pub fn add_token(&mut self, token: &str, raw_ordinal: u64) {
        let lower = token.to_lowercase();
        let max_si = lower.len().min(MAX_CHUNK_BYTES);
        for si in 0..max_si {
            if !lower.is_char_boundary(si) {
                continue;
            }
            let suffix = &lower[si..];
            if si > 0 && suffix.len() < self.min_suffix_len {
                break;
            }
            let prefix = if si == 0 { SI0_PREFIX } else { SI_REST_PREFIX };
            let mut key = String::with_capacity(1 + suffix.len());
            key.push(prefix as char);
            key.push_str(suffix);
            self.entries.push((
                key,
                ParentEntry { raw_ordinal, si: si as u16 },
            ));
        }
    }

    /// Build the FST and output table bytes.
    /// Sorts entries, deduplicates, groups by suffix key, then builds FST.
    pub fn build(mut self) -> Result<(Vec<u8>, Vec<u8>), lucivy_fst::Error> {
        // Sort by suffix key (lexicographic) then by (raw_ordinal, si) for stable dedup
        self.entries.sort_by(|a, b| {
            a.0.cmp(&b.0)
                .then(a.1.raw_ordinal.cmp(&b.1.raw_ordinal))
                .then(a.1.si.cmp(&b.1.si))
        });
        // Deduplicate
        self.entries.dedup_by(|a, b| {
            a.0 == b.0 && a.1.raw_ordinal == b.1.raw_ordinal && a.1.si == b.1.si
        });

        let mut fst_builder = MapBuilder::memory();
        let mut output_table = OutputTableBuilder::new();
        self.num_terms = 0;

        let mut i = 0;
        while i < self.entries.len() {
            let key = &self.entries[i].0;
            // Collect all parents for this suffix key
            let mut j = i + 1;
            while j < self.entries.len() && self.entries[j].0 == *key {
                j += 1;
            }
            let num_parents = j - i;

            let output = if num_parents == 1 {
                let p = &self.entries[i].1;
                encode_single_parent(p.raw_ordinal, p.si)
            } else {
                let parents: Vec<ParentEntry> = self.entries[i..j]
                    .iter()
                    .map(|(_, p)| p.clone())
                    .collect();
                let record = encode_parent_entries(&parents);
                let offset = output_table.add(&record);
                encode_multi_parent(offset)
            };
            fst_builder.insert(key.as_bytes(), output)?;
            self.num_terms += 1;

            i = j;
        }

        let fst_bytes = fst_builder.into_inner()?;
        Ok((fst_bytes, output_table.into_inner()))
    }

    /// Number of unique suffix terms. Only accurate after build().
    /// During accumulation, returns an estimate (total entries, not unique).
    pub fn num_terms(&self) -> usize {
        self.num_terms
    }
}

/// Read a multi-parent list from OutputTable data at the given offset.
/// Kept for backward compatibility — prefer using OutputTable::get() + decode_parent_entries().
pub fn read_parent_list(output_table_data: &[u8], offset: u64) -> Vec<ParentEntry> {
    let table = lucivy_fst::OutputTable::new(output_table_data);
    let record = table.get(offset);
    decode_parent_entries(record)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_decode_single_parent() {
        let encoded = encode_single_parent(42, 3);
        match decode_output(encoded) {
            ParentRef::Single { raw_ordinal, si } => {
                assert_eq!(raw_ordinal, 42);
                assert_eq!(si, 3);
            }
            _ => panic!("expected single parent"),
        }
    }

    #[test]
    fn test_encode_decode_single_parent_max_values() {
        let encoded = encode_single_parent(0x00FF_FFFF, 255);
        match decode_output(encoded) {
            ParentRef::Single { raw_ordinal, si } => {
                assert_eq!(raw_ordinal, 0x00FF_FFFF);
                assert_eq!(si, 255);
            }
            _ => panic!("expected single parent"),
        }
    }

    #[test]
    fn test_encode_decode_multi_parent() {
        let encoded = encode_multi_parent(1234);
        match decode_output(encoded) {
            ParentRef::Multi { offset } => assert_eq!(offset, 1234),
            _ => panic!("expected multi parent"),
        }
    }

    #[test]
    fn test_encode_decode_parent_entries() {
        let entries = vec![
            ParentEntry { raw_ordinal: 5, si: 1 },
            ParentEntry { raw_ordinal: 12, si: 4 },
        ];
        let bytes = encode_parent_entries(&entries);
        let decoded = decode_parent_entries(&bytes);
        assert_eq!(decoded, entries);
    }

    /// Helper: get from FST with prefix byte prepended
    fn fst_get_si0(fst: &lucivy_fst::Map<Vec<u8>>, key: &[u8]) -> Option<u64> {
        let mut prefixed = vec![SI0_PREFIX];
        prefixed.extend_from_slice(key);
        fst.get(&prefixed)
    }

    fn fst_get_si_rest(fst: &lucivy_fst::Map<Vec<u8>>, key: &[u8]) -> Option<u64> {
        let mut prefixed = vec![SI_REST_PREFIX];
        prefixed.extend_from_slice(key);
        fst.get(&prefixed)
    }

    #[test]
    fn test_builder_single_token() {
        let mut builder = SuffixFstBuilder::with_min_suffix_len(3);
        builder.add_token("rag3db", 0);

        let (fst_bytes, _output_table) = builder.build().unwrap();
        let fst = lucivy_fst::Map::new(fst_bytes).unwrap();

        // "rag3db" SI=0 → in \x00 partition
        let val = fst_get_si0(&fst, b"rag3db").expect("rag3db should exist in SI=0");
        assert_eq!(decode_output(val), ParentRef::Single { raw_ordinal: 0, si: 0 });

        // "ag3db" SI=1 → in \x01 partition
        let val = fst_get_si_rest(&fst, b"ag3db").expect("ag3db should exist in SI>0");
        assert_eq!(decode_output(val), ParentRef::Single { raw_ordinal: 0, si: 1 });

        // "g3db" SI=2 → in \x01 partition
        let val = fst_get_si_rest(&fst, b"g3db").expect("g3db should exist in SI>0");
        assert_eq!(decode_output(val), ParentRef::Single { raw_ordinal: 0, si: 2 });

        // "3db" SI=3 → in \x01 partition
        let val = fst_get_si_rest(&fst, b"3db").expect("3db should exist in SI>0");
        assert_eq!(decode_output(val), ParentRef::Single { raw_ordinal: 0, si: 3 });

        // "db" should NOT exist (< min_suffix_len=3)
        assert!(fst_get_si_rest(&fst, b"db").is_none());
        assert!(fst_get_si_rest(&fst, b"b").is_none());
    }

    #[test]
    fn test_builder_multi_parent() {
        let mut builder = SuffixFstBuilder::with_min_suffix_len(3);
        // "core" and "hardcore" both produce suffix "core"
        builder.add_token("core", 0);      // "core" SI=0
        builder.add_token("hardcore", 1);  // "hardcore" has suffix "core" at SI=4

        let (fst_bytes, output_table_data) = builder.build().unwrap();
        let fst = lucivy_fst::Map::new(fst_bytes).unwrap();

        // "core" SI=0 partition: single parent (0, si=0) from "core"
        let val = fst_get_si0(&fst, b"core").expect("core should exist in SI=0");
        assert_eq!(decode_output(val), ParentRef::Single { raw_ordinal: 0, si: 0 });

        // "core" SI>0 partition: single parent (1, si=4) from "hardcore"
        let val = fst_get_si_rest(&fst, b"core").expect("core should exist in SI>0");
        assert_eq!(decode_output(val), ParentRef::Single { raw_ordinal: 1, si: 4 });

        // "hardcore" SI=0 should be single parent
        let val = fst_get_si0(&fst, b"hardcore").expect("hardcore should exist in SI=0");
        assert_eq!(decode_output(val), ParentRef::Single { raw_ordinal: 1, si: 0 });
    }

    #[test]
    fn test_builder_prefix_walk() {
        use lucivy_fst::{IntoStreamer, Streamer};

        let mut builder = SuffixFstBuilder::with_min_suffix_len(3);
        builder.add_token("rag3db", 0);
        builder.add_token("framework", 1);

        let (fst_bytes, _) = builder.build().unwrap();
        let fst = lucivy_fst::Map::new(fst_bytes).unwrap();

        // Prefix walk "\x01g3d" should find "\x01g3db" (SI>0 partition)
        let ge = [SI_REST_PREFIX, b'g', b'3', b'd'];
        let lt = [SI_REST_PREFIX, b'g', b'3', b'e'];
        let mut stream = fst.range().ge(&ge).lt(&lt).into_stream();
        let mut found = Vec::new();
        while let Some((key, val)) = stream.next() {
            // Strip prefix byte
            found.push((String::from_utf8(key[1..].to_vec()).unwrap(), val));
        }
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, "g3db");
        assert_eq!(
            decode_output(found[0].1),
            ParentRef::Single { raw_ordinal: 0, si: 2 }
        );
    }

    #[test]
    fn test_builder_utf8_boundaries() {
        let mut builder = SuffixFstBuilder::with_min_suffix_len(3);
        builder.add_token("café", 0);

        let (fst_bytes, _) = builder.build().unwrap();
        let fst = lucivy_fst::Map::new(fst_bytes).unwrap();

        // "café" SI=0 → \x00 partition
        let val = fst_get_si0(&fst, "café".as_bytes()).expect("café should exist in SI=0");
        assert_eq!(decode_output(val), ParentRef::Single { raw_ordinal: 0, si: 0 });

        // "afé" SI=1 → \x01 partition
        let val = fst_get_si_rest(&fst, "afé".as_bytes()).expect("afé should exist in SI>0");
        assert_eq!(decode_output(val), ParentRef::Single { raw_ordinal: 0, si: 1 });
    }

    #[test]
    fn test_builder_no_duplicate_parents() {
        let mut builder = SuffixFstBuilder::with_min_suffix_len(3);
        builder.add_token("rag3db", 0);
        builder.add_token("rag3db", 0);

        let (fst_bytes, _) = builder.build().unwrap();
        let fst = lucivy_fst::Map::new(fst_bytes).unwrap();

        // Should still be single parent in SI=0 partition
        let val = fst_get_si0(&fst, b"rag3db").unwrap();
        assert!(matches!(decode_output(val), ParentRef::Single { .. }));
    }

    #[test]
    fn test_num_terms() {
        let mut builder = SuffixFstBuilder::with_min_suffix_len(3);
        builder.add_token("rag3db", 0);
        builder.add_token("core", 1);

        // After build with prefix bytes:
        // "rag3db": \x00rag3db, \x01ag3db, \x01g3db, \x013db (4 entries)
        // "core": \x00core, \x01ore (2 entries)
        // Total: 6 unique entries
        let (fst_bytes, _) = builder.build().unwrap();
        let fst = lucivy_fst::Map::new(fst_bytes).unwrap();
        assert_eq!(fst.len(), 6);
    }
}
