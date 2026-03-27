use std::collections::{HashMap, HashSet};

use super::builder::SuffixFstBuilder;
use super::file::SfxFileWriter;
use super::gapmap::GapMapWriter;
use super::sibling_table::SiblingTableWriter;

/// Collects token and gap data during indexation to produce a .sfx file.
///
/// Supports multi-value fields: call begin_value/end_value for each value
/// within a document.
///
/// Usage:
///   collector.begin_doc();
///   collector.begin_value("hello world");
///   collector.add_token("hello", 0, 5);
///   collector.add_token("world", 6, 11);
///   collector.end_value();
///   collector.begin_value("foo bar");
///   collector.add_token("foo", 0, 3);
///   collector.add_token("bar", 4, 7);
///   collector.end_value();
///   collector.end_doc();
pub struct SfxCollector {
    // Interned tokens: each unique token stored once.
    token_intern: HashMap<String, u32>,
    token_texts: Vec<String>,
    // Posting entries indexed by interned ordinal.
    token_postings: Vec<Vec<(u32, u32, u32, u32)>>,
    // Per-segment: gap map writer
    gapmap_writer: GapMapWriter,
    // Sibling pairs: (intern_id, next_intern_id) → set of gap_len values observed.
    // Deduplicated: same pair with same gap_len stored only once.
    sibling_pairs: HashMap<(u32, u32), HashSet<u16>>,

    // Per-document state
    doc_values: Vec<ValueData>,
    doc_active: bool,
    current_doc_id: u32,

    // Per-value state
    current_value_text: Option<String>,
    current_value_tokens: Vec<TokenCapture>,
    current_value_ti_start: u32,

    // Config
    min_suffix_len: usize,
}

/// Accumulated data for one value within a document.
struct ValueData {
    gaps: Vec<Vec<u8>>, // owned gap bytes
    ti_start: u32,
}

/// A captured token — stores interned ordinal and byte offsets.
#[derive(Debug, Clone)]
struct TokenCapture {
    intern_id: u32,
    offset_from: usize,
    offset_to: usize,
}

impl SfxCollector {
    /// Create a new collector. Reads LUCIVY_MIN_SUFFIX_LEN env var (default 1).
    pub fn new() -> Self {
        let min = std::env::var("LUCIVY_MIN_SUFFIX_LEN")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1);
        Self::with_min_suffix_len(min)
    }

    /// Create with custom minimum suffix length.
    pub fn with_min_suffix_len(min_suffix_len: usize) -> Self {
        Self {
            token_intern: HashMap::new(),
            token_texts: Vec::new(),
            token_postings: Vec::new(),
            gapmap_writer: GapMapWriter::new(),
            sibling_pairs: HashMap::new(),
            doc_values: Vec::new(),
            doc_active: false,
            current_doc_id: 0,
            current_value_text: None,
            current_value_tokens: Vec::new(),
            current_value_ti_start: 0,
            min_suffix_len,
        }
    }

    /// Begin processing a new document.
    pub fn begin_doc(&mut self) {
        self.doc_values.clear();
        self.doc_active = true;
        self.current_value_ti_start = 0;
    }

    /// Begin a new value within the current document.
    /// `raw_text` is the original text string for this value.
    /// Token index is tracked internally (cumulative across values in a doc).
    pub fn begin_value(&mut self, raw_text: &str) {
        self.current_value_text = Some(raw_text.to_string());
        self.current_value_tokens.clear();
    }

    /// Intern a token, returning its ordinal. Allocates only on first occurrence.
    #[inline]
    fn intern_token(&mut self, text: &str) -> u32 {
        if let Some(&ord) = self.token_intern.get(text) {
            return ord;
        }
        let ord = self.token_texts.len() as u32;
        self.token_intern.insert(text.to_string(), ord);
        self.token_texts.push(text.to_string());
        self.token_postings.push(Vec::new());
        ord
    }

    /// Add a token from the current value's tokenization.
    /// Tokens exceeding MAX_TOKEN_LEN are skipped (consistent with postings_writer).
    pub fn add_token(&mut self, text: &str, offset_from: usize, offset_to: usize) {
        if text.len() > crate::tokenizer::MAX_TOKEN_LEN {
            return;
        }
        let ti = self.current_value_ti_start + self.current_value_tokens.len() as u32;
        let ord = self.intern_token(text);
        self.token_postings[ord as usize].push((
            self.current_doc_id, ti, offset_from as u32, offset_to as u32,
        ));
        self.current_value_tokens.push(TokenCapture { intern_id: ord, offset_from, offset_to });
    }

    /// End the current value. Computes gaps and sibling links from the raw text and captured tokens.
    pub fn end_value(&mut self) {
        let text = self.current_value_text.take().unwrap_or_default();
        let tokens = std::mem::take(&mut self.current_value_tokens);

        // Collect sibling pairs: consecutive tokens with their gap lengths.
        for i in 0..tokens.len().saturating_sub(1) {
            let gap_len = tokens[i + 1].offset_from.saturating_sub(tokens[i].offset_to);
            let gap_len_u16 = gap_len.min(u16::MAX as usize) as u16;
            self.sibling_pairs
                .entry((tokens[i].intern_id, tokens[i + 1].intern_id))
                .or_default()
                .insert(gap_len_u16);
        }

        let gaps = if tokens.is_empty() {
            // No tokens in this value — just store empty prefix+suffix
            vec![Vec::new(), Vec::new()]
        } else {
            let text_bytes = text.as_bytes();
            let mut gaps = Vec::with_capacity(tokens.len() + 1);

            // prefix before first token
            let first_start = tokens[0].offset_from;
            if first_start <= text_bytes.len() {
                gaps.push(text_bytes[..first_start].to_vec());
            } else {
                gaps.push(Vec::new());
            }

            // separators between consecutive tokens
            for i in 1..tokens.len() {
                let prev_end = tokens[i - 1].offset_to;
                let curr_start = tokens[i].offset_from;
                if prev_end <= curr_start && curr_start <= text_bytes.len() {
                    gaps.push(text_bytes[prev_end..curr_start].to_vec());
                } else {
                    // Overlapping or out-of-order offsets: empty gap.
                    gaps.push(Vec::new());
                }
            }

            // suffix after last token
            let last_end = tokens.last().unwrap().offset_to;
            if last_end <= text_bytes.len() {
                gaps.push(text_bytes[last_end..].to_vec());
            } else {
                gaps.push(Vec::new());
            }

            gaps
        };

        self.doc_values.push(ValueData {
            gaps,
            ti_start: self.current_value_ti_start,
        });
        // Advance cumulative token counter: tokens + 1 boundary gap between values.
        // The gap prevents phrase queries from matching across value boundaries.
        self.current_value_ti_start += tokens.len() as u32 + 1;
    }

    /// End the current document. Writes accumulated value gaps to the GapMap.
    pub fn end_doc(&mut self) {
        self.doc_active = false;
        self.current_doc_id += 1;

        if self.doc_values.is_empty() {
            self.gapmap_writer.add_empty_doc();
            return;
        }

        if self.doc_values.len() == 1 {
            // Single-value fast path
            let value = &self.doc_values[0];
            let gap_refs: Vec<&[u8]> = value.gaps.iter().map(|g| g.as_slice()).collect();
            self.gapmap_writer.add_doc(&gap_refs);
        } else {
            // Multi-value
            let values_gaps: Vec<Vec<&[u8]>> = self
                .doc_values
                .iter()
                .map(|v| v.gaps.iter().map(|g| g.as_slice()).collect())
                .collect();
            let ti_starts: Vec<u32> = self.doc_values.iter().map(|v| v.ti_start).collect();
            self.gapmap_writer.add_doc_multi(&values_gaps, &ti_starts);
        }
    }

    /// End doc without any values — adds an empty doc to gapmap.
    pub fn end_doc_empty(&mut self) {
        self.doc_active = false;
        self.current_doc_id += 1;
        self.gapmap_writer.add_empty_doc();
    }

    /// Build the .sfx file bytes and the .sfxpost file bytes.
    ///
    /// Returns `(sfx_bytes, sfxpost_bytes)`:
    /// - sfx_bytes: suffix FST + parent lists + GapMap
    /// - sfxpost_bytes: posting index (ordinal → delta-VInt doc IDs)
    pub fn build(self) -> Result<(Vec<u8>, Vec<u8>), lucivy_fst::Error> {
        // Sort interned tokens to get BTreeSet-equivalent order for ordinals.
        let num_tokens = self.token_texts.len();
        let mut sorted_indices: Vec<u32> = (0..num_tokens as u32).collect();
        sorted_indices.sort_by(|&a, &b| {
            self.token_texts[a as usize].cmp(&self.token_texts[b as usize])
        });
        // sorted_indices[new_ordinal] = old_intern_ordinal

        let mut sfx_builder = SuffixFstBuilder::with_min_suffix_len(self.min_suffix_len);
        for (new_ordinal, &old_ord) in sorted_indices.iter().enumerate() {
            sfx_builder.add_token(&self.token_texts[old_ord as usize], new_ordinal as u64);
        }

        let (fst_data, parent_list_data) = sfx_builder.build()?;
        let num_terms = num_tokens as u32;
        let gapmap_data = self.gapmap_writer.serialize();

        // Build reverse mapping: intern_id → final_ordinal
        let mut intern_to_final = vec![0u32; num_tokens];
        for (new_ord, &old_ord) in sorted_indices.iter().enumerate() {
            intern_to_final[old_ord as usize] = new_ord as u32;
        }

        // Build sibling table with remapped ordinals
        let mut sibling_writer = SiblingTableWriter::new(num_terms);
        for ((intern_a, intern_b), gap_lens) in &self.sibling_pairs {
            let final_a = intern_to_final[*intern_a as usize];
            let final_b = intern_to_final[*intern_b as usize];
            for &gap_len in gap_lens {
                sibling_writer.add(final_a, final_b, gap_len);
            }
        }
        let sibling_data = sibling_writer.serialize();

        let file_writer = SfxFileWriter::new(
            fst_data,
            parent_list_data,
            gapmap_data,
            self.gapmap_writer.num_docs(),
            num_terms,
        ).with_sibling_data(sibling_data);
        let sfx_bytes = file_writer.to_bytes();

        // .sfxpost file V2: per-ordinal posting entries with binary-searchable doc_ids.
        let mut sfxpost_writer = super::sfxpost_v2::SfxPostWriterV2::new(num_tokens);
        for (new_ord, &old_ord) in sorted_indices.iter().enumerate() {
            let entries = &self.token_postings[old_ord as usize];
            for &(doc_id, ti, byte_from, byte_to) in entries {
                sfxpost_writer.add_entry(new_ord as u32, doc_id, ti, byte_from, byte_to);
            }
        }
        let sfxpost_bytes = sfxpost_writer.finish();

        Ok((sfx_bytes, sfxpost_bytes))
    }
}

/// Encode a u32 as a variable-length integer (1-5 bytes, little-endian, MSB continuation).
pub(crate) fn encode_vint(mut val: u32, out: &mut Vec<u8>) {
    loop {
        let byte = (val & 0x7F) as u8;
        val >>= 7;
        if val == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suffix_fst::file::SfxFileReader;

    #[test]
    fn test_collector_single_value() {
        let mut collector = SfxCollector::new();

        collector.begin_doc();
        collector.begin_value("import rag3db from 'rag3db_core';");
        collector.add_token("import", 0, 6);
        collector.add_token("rag3db", 7, 13);
        collector.add_token("from", 14, 18);
        collector.add_token("rag3db", 20, 26);
        collector.add_token("core", 27, 31);
        collector.end_value();
        collector.end_doc();

        let (sfx_bytes, _sfxpost_bytes) = collector.build().unwrap();
        let reader = SfxFileReader::open(&sfx_bytes).unwrap();

        // Verify suffix resolution
        let parents = reader.resolve_suffix("rag3db");
        assert!(!parents.is_empty(), "should find 'rag3db'");
        assert!(parents.iter().any(|p| p.si == 0), "SI=0 entry");

        // Verify substring
        let parents = reader.resolve_suffix("g3db");
        assert!(!parents.is_empty(), "should find suffix 'g3db'");
        assert!(parents.iter().all(|p| p.si > 0), "all SI>0");

        // GapMap: 1 doc, 5 tokens
        assert_eq!(reader.gapmap().num_tokens(0), 5);
        // Gap between tokens 0 and 1: " " (space between "import" and "rag3db")
        assert_eq!(reader.gapmap().read_separator(0, 0, 1), Some(b" ".as_slice()));
        // Gap after last token: "';", which is bytes 31..32 of the original text
        assert_eq!(reader.gapmap().read_gap(0, 5), b"';");
    }

    #[test]
    fn test_collector_multi_value() {
        let mut collector = SfxCollector::new();

        collector.begin_doc();
        // Value 0: "hello world" → Ti=0,1 → end_position=2+GAP=3
        collector.begin_value("hello world");
        collector.add_token("hello", 0, 5);
        collector.add_token("world", 6, 11);
        collector.end_value();
        // Value 1: "foo bar" → Ti=3,4
        collector.begin_value("foo bar");
        collector.add_token("foo", 0, 3);
        collector.add_token("bar", 4, 7);
        collector.end_value();
        collector.end_doc();

        let (sfx_bytes, _sfxpost_bytes) = collector.build().unwrap();
        let reader = SfxFileReader::open(&sfx_bytes).unwrap();

        assert_eq!(reader.gapmap().num_tokens(0), 4);
        assert_eq!(reader.gapmap().num_values(0), 2);

        // Within value 0: separator " "
        assert_eq!(
            reader.gapmap().read_separator(0, 0, 1),
            Some(b" ".as_slice())
        );
        // Value boundary between value 0 (ti=0,1) and value 1 (ti=3,4):
        // Ti 2 is a gap slot, not a real token. Consecutive token pairs
        // across the boundary (ti=1 → ti=3) are not adjacent, so
        // read_separator returns None (ti_b != ti_a + 1).
        assert_eq!(reader.gapmap().read_separator(0, 1, 3), None);
        // Within value 1: separator " " between ti=3 and ti=4
        assert_eq!(
            reader.gapmap().read_separator(0, 3, 4),
            Some(b" ".as_slice())
        );
    }

    #[test]
    fn test_collector_empty_docs() {
        let mut collector = SfxCollector::new();

        collector.begin_doc();
        collector.begin_value("hello world");
        collector.add_token("hello", 0, 5);
        collector.add_token("world", 6, 11);
        collector.end_value();
        collector.end_doc();

        collector.begin_doc();
        collector.begin_value("foo");
        collector.add_token("foo", 0, 3);
        collector.end_value();
        collector.end_doc();

        collector.begin_doc();
        collector.end_doc_empty();

        let (sfx_bytes, _sfxpost_bytes) = collector.build().unwrap();
        let reader = SfxFileReader::open(&sfx_bytes).unwrap();

        assert_eq!(reader.gapmap().num_tokens(0), 2);
        assert_eq!(reader.gapmap().num_tokens(1), 1);
        assert_eq!(reader.gapmap().num_tokens(2), 0);
    }

    #[test]
    fn test_collector_prefix_walk() {
        let mut collector = SfxCollector::new();

        collector.begin_doc();
        collector.begin_value("framework");
        collector.add_token("framework", 0, 9);
        collector.end_value();
        collector.end_doc();

        let (sfx_bytes, _sfxpost_bytes) = collector.build().unwrap();
        let reader = SfxFileReader::open(&sfx_bytes).unwrap();

        let results = reader.prefix_walk("work");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].1[0].si, 5);
    }

    #[test]
    fn test_collector_ordinals_match_sorted_tokens() {
        let mut collector = SfxCollector::new();

        collector.begin_doc();
        collector.begin_value("zebra apple mango");
        collector.add_token("zebra", 0, 5);
        collector.add_token("apple", 6, 11);
        collector.add_token("mango", 12, 17);
        collector.end_value();
        collector.end_doc();

        let (sfx_bytes, _sfxpost_bytes) = collector.build().unwrap();
        let reader = SfxFileReader::open(&sfx_bytes).unwrap();

        // Tokens should be sorted: apple=0, mango=1, zebra=2
        let parents_a = reader.resolve_suffix("apple");
        let parents_m = reader.resolve_suffix("mango");
        let parents_z = reader.resolve_suffix("zebra");

        let ord_a = parents_a.iter().find(|p| p.si == 0).unwrap().raw_ordinal;
        let ord_m = parents_m.iter().find(|p| p.si == 0).unwrap().raw_ordinal;
        let ord_z = parents_z.iter().find(|p| p.si == 0).unwrap().raw_ordinal;

        assert!(ord_a < ord_m, "apple ({ord_a}) < mango ({ord_m})");
        assert!(ord_m < ord_z, "mango ({ord_m}) < zebra ({ord_z})");
    }

    #[test]
    fn test_collector_sibling_links() {
        let mut collector = SfxCollector::new();
        collector.begin_doc();
        // "rag3Weaver" → CamelCaseSplit → "rag3"(0,4) + "weaver"(4,10) — contiguous
        collector.begin_value("rag3Weaver");
        collector.add_token("rag3", 0, 4);
        collector.add_token("weaver", 4, 10);
        collector.end_value();
        collector.end_doc();

        collector.begin_doc();
        // "import rag3db" → "import"(0,6) + "rag3db"(7,13) — gap=1 (space)
        collector.begin_value("import rag3db");
        collector.add_token("import", 0, 6);
        collector.add_token("rag3db", 7, 13);
        collector.end_value();
        collector.end_doc();

        let (sfx_bytes, _) = collector.build().unwrap();
        let reader = SfxFileReader::open(&sfx_bytes).unwrap();
        let sibling = reader.sibling_table().expect("sibling table should be present");

        // Sorted tokens: import(0), rag3(1), rag3db(2), weaver(3)
        // Sibling links:
        //   rag3(1) → weaver(3), gap=0 (contiguous)
        //   import(0) → rag3db(2), gap=1 (space)

        let rag3_siblings = sibling.siblings(1); // rag3
        assert_eq!(rag3_siblings.len(), 1);
        assert_eq!(rag3_siblings[0].next_ordinal, 3); // weaver
        assert_eq!(rag3_siblings[0].gap_len, 0);

        let import_siblings = sibling.siblings(0); // import
        assert_eq!(import_siblings.len(), 1);
        assert_eq!(import_siblings[0].next_ordinal, 2); // rag3db
        assert_eq!(import_siblings[0].gap_len, 1);

        // Contiguous only
        let contiguous = sibling.contiguous_siblings(1); // rag3
        assert_eq!(contiguous, vec![3]); // weaver

        let contiguous_import = sibling.contiguous_siblings(0); // import
        assert!(contiguous_import.is_empty()); // gap=1 → not contiguous
    }
}
