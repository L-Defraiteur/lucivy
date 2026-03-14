use std::collections::BTreeSet;

use super::builder::SuffixFstBuilder;
use super::file::SfxFileWriter;
use super::gapmap::GapMapWriter;

/// Collects token and gap data during indexation to produce a .sfx file.
///
/// Supports multi-value fields: call begin_value/end_value for each value
/// within a document.
///
/// Usage:
///   collector.begin_doc();
///   collector.begin_value("hello world", 0);
///   collector.add_token("hello", 0, 5);
///   collector.add_token("world", 6, 11);
///   collector.end_value();
///   collector.begin_value("foo bar", 3);
///   collector.add_token("foo", 0, 3);
///   collector.add_token("bar", 4, 7);
///   collector.end_value();
///   collector.end_doc();
pub struct SfxCollector {
    // Per-segment: unique raw tokens
    unique_tokens: BTreeSet<String>,
    // Per-segment: gap map writer
    gapmap_writer: GapMapWriter,

    // Per-document state
    doc_values: Vec<ValueData>,
    doc_active: bool,

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

/// A captured token.
#[derive(Debug, Clone)]
struct TokenCapture {
    text: String,
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
            unique_tokens: BTreeSet::new(),
            gapmap_writer: GapMapWriter::new(),
            doc_values: Vec::new(),
            doc_active: false,
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
    /// `ti_start` is the posting Ti of the first token of this value
    /// (= indexing_position.end_position before tokenizing this value).
    pub fn begin_value(&mut self, raw_text: &str, ti_start: u32) {
        self.current_value_text = Some(raw_text.to_string());
        self.current_value_tokens.clear();
        self.current_value_ti_start = ti_start;
    }

    /// Add a token from the current value's tokenization.
    pub fn add_token(&mut self, text: &str, offset_from: usize, offset_to: usize) {
        self.unique_tokens.insert(text.to_string());
        self.current_value_tokens.push(TokenCapture {
            text: text.to_string(),
            offset_from,
            offset_to,
        });
    }

    /// End the current value. Computes gaps from the raw text and captured tokens.
    pub fn end_value(&mut self) {
        let text = self.current_value_text.take().unwrap_or_default();
        let tokens = std::mem::take(&mut self.current_value_tokens);

        let gaps = if tokens.is_empty() {
            // No tokens in this value — just store empty prefix+suffix
            vec![Vec::new(), Vec::new()]
        } else {
            let text_bytes = text.as_bytes();
            let mut gaps = Vec::with_capacity(tokens.len() + 1);

            // prefix before first token
            gaps.push(text_bytes[..tokens[0].offset_from].to_vec());

            // separators between consecutive tokens
            for i in 1..tokens.len() {
                let prev_end = tokens[i - 1].offset_to;
                let curr_start = tokens[i].offset_from;
                gaps.push(text_bytes[prev_end..curr_start].to_vec());
            }

            // suffix after last token
            gaps.push(text_bytes[tokens.last().unwrap().offset_to..].to_vec());

            gaps
        };

        self.doc_values.push(ValueData {
            gaps,
            ti_start: self.current_value_ti_start,
        });
    }

    /// End the current document. Writes accumulated value gaps to the GapMap.
    pub fn end_doc(&mut self) {
        self.doc_active = false;

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
        self.gapmap_writer.add_empty_doc();
    }

    /// Build the .sfx file bytes from all collected data.
    pub fn build(self) -> Result<Vec<u8>, lucivy_fst::Error> {
        let mut sfx_builder = SuffixFstBuilder::with_min_suffix_len(self.min_suffix_len);
        for (ordinal, token) in self.unique_tokens.iter().enumerate() {
            sfx_builder.add_token(token, ordinal as u64);
        }

        let num_terms = sfx_builder.num_terms() as u32;
        let (fst_data, parent_list_data) = sfx_builder.build()?;
        let gapmap_data = self.gapmap_writer.serialize();

        let file_writer = SfxFileWriter::new(
            fst_data,
            parent_list_data,
            gapmap_data,
            self.gapmap_writer.num_docs(),
            num_terms,
        );

        Ok(file_writer.to_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suffix_fst::builder::ParentEntry;
    use crate::suffix_fst::file::SfxFileReader;
    use crate::suffix_fst::gapmap::is_value_boundary;

    #[test]
    fn test_collector_single_value() {
        let mut collector = SfxCollector::new();

        collector.begin_doc();
        collector.begin_value("import rag3db from 'rag3db_core';", 0);
        collector.add_token("import", 0, 6);
        collector.add_token("rag3db", 7, 13);
        collector.add_token("from", 14, 18);
        collector.add_token("rag3db", 20, 26);
        collector.add_token("core", 27, 31);
        collector.end_value();
        collector.end_doc();

        let sfx_bytes = collector.build().unwrap();
        let reader = SfxFileReader::open(&sfx_bytes).unwrap();

        let parents = reader.resolve_suffix("g3db");
        assert_eq!(parents.len(), 1);
        assert_eq!(parents[0].si, 2);

        assert_eq!(reader.gapmap().num_tokens(0), 5);
        assert_eq!(reader.gapmap().num_values(0), 1);
        assert_eq!(reader.gapmap().read_gap(0, 0), b"");
        assert_eq!(reader.gapmap().read_gap(0, 1), b" ");
        assert_eq!(reader.gapmap().read_gap(0, 3), b" '");
        assert_eq!(reader.gapmap().read_gap(0, 4), b"_");
        assert_eq!(reader.gapmap().read_gap(0, 5), b"';");
    }

    #[test]
    fn test_collector_multi_value() {
        let mut collector = SfxCollector::new();

        collector.begin_doc();
        // Value 0: "hello world" → Ti=0,1 → end_position=2+GAP=3
        collector.begin_value("hello world", 0);
        collector.add_token("hello", 0, 5);
        collector.add_token("world", 6, 11);
        collector.end_value();
        // Value 1: "foo bar" → Ti=3,4
        collector.begin_value("foo bar", 3);
        collector.add_token("foo", 0, 3);
        collector.add_token("bar", 4, 7);
        collector.end_value();
        collector.end_doc();

        let sfx_bytes = collector.build().unwrap();
        let reader = SfxFileReader::open(&sfx_bytes).unwrap();

        assert_eq!(reader.gapmap().num_tokens(0), 4);
        assert_eq!(reader.gapmap().num_values(0), 2);

        // Within value 0: separator " "
        assert_eq!(
            reader.gapmap().read_separator(0, 0, 1),
            Some(b" ".as_slice())
        );
        // Cross value: Ti=1→Ti=3, not consecutive → None
        assert_eq!(reader.gapmap().read_separator(0, 1, 3), None);
        // Within value 1: separator " "
        assert_eq!(
            reader.gapmap().read_separator(0, 3, 4),
            Some(b" ".as_slice())
        );

        // All gaps include VALUE_BOUNDARY
        let all_gaps = reader.gapmap().read_all_gaps(0);
        assert!(all_gaps.iter().any(|g| is_value_boundary(g)));
    }

    #[test]
    fn test_collector_multi_docs() {
        let mut collector = SfxCollector::new();

        collector.begin_doc();
        collector.begin_value("hello world", 0);
        collector.add_token("hello", 0, 5);
        collector.add_token("world", 6, 11);
        collector.end_value();
        collector.end_doc();

        collector.begin_doc();
        collector.begin_value("foo", 0);
        collector.add_token("foo", 0, 3);
        collector.end_value();
        collector.end_doc();

        collector.begin_doc();
        collector.end_doc_empty();

        let sfx_bytes = collector.build().unwrap();
        let reader = SfxFileReader::open(&sfx_bytes).unwrap();

        assert_eq!(reader.num_docs(), 3);
        assert_eq!(reader.gapmap().num_tokens(0), 2);
        assert_eq!(reader.gapmap().num_tokens(1), 1);
        assert_eq!(reader.gapmap().num_tokens(2), 0);
    }

    #[test]
    fn test_collector_prefix_walk() {
        let mut collector = SfxCollector::new();

        collector.begin_doc();
        collector.begin_value("framework", 0);
        collector.add_token("framework", 0, 9);
        collector.end_value();
        collector.end_doc();

        let sfx_bytes = collector.build().unwrap();
        let reader = SfxFileReader::open(&sfx_bytes).unwrap();

        let results = reader.prefix_walk("work");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "work");
        assert_eq!(results[0].1[0].si, 5);
    }

    #[test]
    fn test_collector_ordinals_match_sorted_tokens() {
        let mut collector = SfxCollector::new();

        collector.begin_doc();
        collector.begin_value("zebra apple mango", 0);
        collector.add_token("zebra", 0, 5);
        collector.add_token("apple", 6, 11);
        collector.add_token("mango", 12, 17);
        collector.end_value();
        collector.end_doc();

        let sfx_bytes = collector.build().unwrap();
        let reader = SfxFileReader::open(&sfx_bytes).unwrap();

        let parents = reader.resolve_suffix("apple");
        assert_eq!(parents[0], ParentEntry { raw_ordinal: 0, si: 0 });
        let parents = reader.resolve_suffix("mango");
        assert_eq!(parents[0], ParentEntry { raw_ordinal: 1, si: 0 });
        let parents = reader.resolve_suffix("zebra");
        assert_eq!(parents[0], ParentEntry { raw_ordinal: 2, si: 0 });
    }
}
