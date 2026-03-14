use std::collections::BTreeMap;
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
//   bits 24-31 = SI (up to 256 chars)
//
// For multi-parent:
//   bit 63 = 1
//   bits 0-31  = offset into OutputTable

const MULTI_PARENT_FLAG: u64 = 1 << 63;
const RAW_ORDINAL_MASK: u64 = 0x00FF_FFFF; // 24 bits
const SI_SHIFT: u32 = 24;
const SI_MASK: u64 = 0xFF; // 8 bits

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
    let mut buf = Vec::with_capacity(1 + sorted.len() * 6);
    buf.push(sorted.len() as u8);
    for p in &sorted {
        buf.extend_from_slice(&(p.raw_ordinal as u32).to_le_bytes());
        buf.extend_from_slice(&p.si.to_le_bytes());
    }
    buf
}

/// Decode parent entries from bytes read from the OutputTable.
pub fn decode_parent_entries(data: &[u8]) -> Vec<ParentEntry> {
    let num_parents = data[0] as usize;
    let mut cursor = 1;
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
/// The builder accumulates (suffix_term -> parent list) mappings. Each token's
/// suffixes of length >= min_suffix_len are registered. The builder only stores
/// unique terms and their parents, not per-document occurrences (those live in ._raw).
///
/// At build time, produces:
/// - FST bytes: suffix term -> u64 output (encoded parent ref)
/// - OutputTable bytes: for multi-parent suffixes, variable-length records
///   containing packed (raw_ordinal, SI) entries
pub struct SuffixFstBuilder {
    suffix_to_parents: BTreeMap<String, Vec<ParentEntry>>,
    min_suffix_len: usize,
}

impl SuffixFstBuilder {
    pub fn new() -> Self {
        Self::with_min_suffix_len(default_min_suffix_len())
    }

    pub fn with_min_suffix_len(min: usize) -> Self {
        Self {
            suffix_to_parents: BTreeMap::new(),
            min_suffix_len: min,
        }
    }

    /// Register all suffixes of a token. Called once per unique token in the segment.
    /// `raw_ordinal` is the term ordinal in the ._raw FST (sorted alphabetical position).
    pub fn add_token(&mut self, token: &str, raw_ordinal: u64) {
        let lower = token.to_lowercase();
        for si in 0..lower.len() {
            if !lower.is_char_boundary(si) {
                continue;
            }
            let suffix = &lower[si..];
            // SI=0 (full token) is always indexed regardless of length.
            // SI>0 (true suffix) is only indexed if long enough.
            if si > 0 && suffix.len() < self.min_suffix_len {
                break; // remaining suffixes are even shorter
            }
            let entry = ParentEntry {
                raw_ordinal,
                si: si as u16,
            };
            let parents = self.suffix_to_parents.entry(suffix.to_string()).or_default();
            // Deduplicate: same token shouldn't be added twice
            if !parents.iter().any(|p| p.raw_ordinal == raw_ordinal && p.si == entry.si) {
                parents.push(entry);
            }
        }
    }

    /// Build the FST and output table bytes.
    /// Returns (fst_bytes, output_table_bytes).
    pub fn build(self) -> Result<(Vec<u8>, Vec<u8>), lucivy_fst::Error> {
        let mut fst_builder = MapBuilder::memory();
        let mut output_table = OutputTableBuilder::new();

        for (suffix, parents) in &self.suffix_to_parents {
            let output = if parents.len() == 1 {
                let p = &parents[0];
                encode_single_parent(p.raw_ordinal, p.si)
            } else {
                let record = encode_parent_entries(parents);
                let offset = output_table.add(&record);
                encode_multi_parent(offset)
            };
            fst_builder.insert(suffix.as_bytes(), output)?;
        }

        let fst_bytes = fst_builder.into_inner()?;
        Ok((fst_bytes, output_table.into_inner()))
    }

    /// Number of unique suffix terms accumulated so far.
    pub fn num_terms(&self) -> usize {
        self.suffix_to_parents.len()
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

    #[test]
    fn test_builder_single_token() {
        let mut builder = SuffixFstBuilder::with_min_suffix_len(3);
        builder.add_token("rag3db", 0);

        let (fst_bytes, _output_table) = builder.build().unwrap();
        let fst = lucivy_fst::Map::new(fst_bytes).unwrap();

        // "rag3db" SI=0
        let val = fst.get(b"rag3db").expect("rag3db should exist");
        assert_eq!(decode_output(val), ParentRef::Single { raw_ordinal: 0, si: 0 });

        // "ag3db" SI=1
        let val = fst.get(b"ag3db").expect("ag3db should exist");
        assert_eq!(decode_output(val), ParentRef::Single { raw_ordinal: 0, si: 1 });

        // "g3db" SI=2
        let val = fst.get(b"g3db").expect("g3db should exist");
        assert_eq!(decode_output(val), ParentRef::Single { raw_ordinal: 0, si: 2 });

        // "3db" SI=3
        let val = fst.get(b"3db").expect("3db should exist");
        assert_eq!(decode_output(val), ParentRef::Single { raw_ordinal: 0, si: 3 });

        // "db" should NOT exist (< min_suffix_len=3)
        assert!(fst.get(b"db").is_none());
        assert!(fst.get(b"b").is_none());
    }

    #[test]
    fn test_builder_multi_parent() {
        let mut builder = SuffixFstBuilder::with_min_suffix_len(3);
        // "core" and "hardcore" both produce suffix "core"
        builder.add_token("core", 0);      // "core" SI=0
        builder.add_token("hardcore", 1);  // "hardcore" has suffix "core" at SI=4

        let (fst_bytes, output_table_data) = builder.build().unwrap();
        let fst = lucivy_fst::Map::new(fst_bytes).unwrap();

        // "core" has 2 parents: (0, SI=0) from "core" and (1, SI=4) from "hardcore"
        let val = fst.get(b"core").expect("core should exist");
        match decode_output(val) {
            ParentRef::Multi { offset } => {
                let entries = read_parent_list(&output_table_data, offset);
                assert_eq!(entries.len(), 2);
                assert!(entries.contains(&ParentEntry { raw_ordinal: 0, si: 0 }));
                assert!(entries.contains(&ParentEntry { raw_ordinal: 1, si: 4 }));
            }
            _ => panic!("expected multi parent for 'core'"),
        }

        // "hardcore" SI=0 should be single parent
        let val = fst.get(b"hardcore").expect("hardcore should exist");
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

        // Prefix walk "g3d" should find "g3db"
        let mut stream = fst.range().ge(b"g3d").lt(b"g3e").into_stream();
        let mut found = Vec::new();
        while let Some((key, val)) = stream.next() {
            found.push((String::from_utf8(key.to_vec()).unwrap(), val));
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
        // "café" has a multi-byte char 'é' (2 bytes in UTF-8)
        builder.add_token("café", 0);

        let (fst_bytes, _) = builder.build().unwrap();
        let fst = lucivy_fst::Map::new(fst_bytes).unwrap();

        // Should have "café" (SI=0) and "afé" (SI=1, but byte index varies)
        // "café" in lowercase is "café", 5 bytes: c(1) a(1) f(1) é(2)
        let val = fst.get("café".as_bytes()).expect("café should exist");
        assert_eq!(decode_output(val), ParentRef::Single { raw_ordinal: 0, si: 0 });

        // "afé" SI=1
        let val = fst.get("afé".as_bytes()).expect("afé should exist");
        assert_eq!(decode_output(val), ParentRef::Single { raw_ordinal: 0, si: 1 });

        // Should NOT have a suffix starting in the middle of 'é'
        // (is_char_boundary check filters it)
    }

    #[test]
    fn test_builder_no_duplicate_parents() {
        let mut builder = SuffixFstBuilder::with_min_suffix_len(3);
        // Adding same token twice should not duplicate parent entries
        builder.add_token("rag3db", 0);
        builder.add_token("rag3db", 0);

        let (fst_bytes, _) = builder.build().unwrap();
        let fst = lucivy_fst::Map::new(fst_bytes).unwrap();

        // Should still be single parent, not multi
        let val = fst.get(b"rag3db").unwrap();
        assert!(matches!(decode_output(val), ParentRef::Single { .. }));
    }

    #[test]
    fn test_num_terms() {
        let mut builder = SuffixFstBuilder::with_min_suffix_len(3);
        builder.add_token("rag3db", 0);
        // "rag3db"(6) - min_suffix_len(3) = suffixes: rag3db, ag3db, g3db, 3db = 4 terms
        assert_eq!(builder.num_terms(), 4);

        builder.add_token("core", 1);
        // "core"(4): core, ore = 2 new terms (but "ore" is new, "core" is new)
        // Total: 4 + 2 = 6
        assert_eq!(builder.num_terms(), 6);
    }
}
