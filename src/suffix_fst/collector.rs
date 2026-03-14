use std::collections::BTreeSet;

use super::builder::SuffixFstBuilder;
use super::file::SfxFileWriter;
use super::gapmap::GapMapWriter;

/// Collects token and gap data during indexation to produce a .sfx file.
///
/// Portable and self-contained — can be moved to a separate thread/pipeline later.
/// Currently fed by SegmentWriter during ._raw field processing.
///
/// Usage:
///   1. For each document: call `begin_doc()`, then `add_token()` for each token,
///      then `end_doc(text)` with the raw text to extract gaps.
///   2. After all documents: call `build()` to produce the .sfx file bytes.
pub struct SfxCollector {
    // Per-segment: unique raw tokens and their sorted ordinals
    unique_tokens: BTreeSet<String>,

    // Per-segment: gap map writer
    gapmap_writer: GapMapWriter,

    // Per-document accumulator: tokens seen in current doc
    current_doc_tokens: Vec<TokenCapture>,

    // Config
    min_suffix_len: usize,
}

/// A captured token from the ._raw field tokenization.
#[derive(Debug, Clone)]
struct TokenCapture {
    text: String,         // lowercase token text
    offset_from: usize,   // byte offset in original text
    offset_to: usize,     // byte offset in original text
}

impl SfxCollector {
    /// Create a new collector with default min_suffix_len=3.
    pub fn new() -> Self {
        Self::with_min_suffix_len(3)
    }

    /// Create with custom minimum suffix length.
    pub fn with_min_suffix_len(min_suffix_len: usize) -> Self {
        Self {
            unique_tokens: BTreeSet::new(),
            gapmap_writer: GapMapWriter::new(),
            current_doc_tokens: Vec::new(),
            min_suffix_len,
        }
    }

    /// Begin processing a new document. Must be called before add_token().
    pub fn begin_doc(&mut self) {
        self.current_doc_tokens.clear();
    }

    /// Add a token from the ._raw field tokenization.
    /// Called for each token in the order they appear in the text.
    pub fn add_token(&mut self, text: &str, offset_from: usize, offset_to: usize) {
        self.unique_tokens.insert(text.to_string());
        self.current_doc_tokens.push(TokenCapture {
            text: text.to_string(),
            offset_from,
            offset_to,
        });
    }

    /// End the current document. Extracts gaps from the raw text using token offsets.
    /// `raw_text` is the original text string that was tokenized.
    pub fn end_doc(&mut self, raw_text: &str) {
        if self.current_doc_tokens.is_empty() {
            self.gapmap_writer.add_empty_doc();
            return;
        }

        let mut gaps: Vec<&[u8]> = Vec::with_capacity(self.current_doc_tokens.len() + 1);
        let text_bytes = raw_text.as_bytes();

        // gap[0] = prefix before first token
        let first_offset = self.current_doc_tokens[0].offset_from;
        gaps.push(&text_bytes[..first_offset]);

        // gap[i] = separator between token i-1 and token i
        for i in 1..self.current_doc_tokens.len() {
            let prev_end = self.current_doc_tokens[i - 1].offset_to;
            let curr_start = self.current_doc_tokens[i].offset_from;
            gaps.push(&text_bytes[prev_end..curr_start]);
        }

        // gap[N] = suffix after last token
        let last_offset = self.current_doc_tokens.last().unwrap().offset_to;
        gaps.push(&text_bytes[last_offset..]);

        self.gapmap_writer.add_doc(&gaps);
    }

    /// End doc without raw text — adds an empty doc to gapmap.
    /// Used when the field is not present or is pre-tokenized without raw text access.
    pub fn end_doc_empty(&mut self) {
        self.gapmap_writer.add_empty_doc();
    }

    /// Build the .sfx file bytes from all collected data.
    ///
    /// This consumes the collector. The raw ordinals are derived from the sorted
    /// unique token set (same order as the ._raw FST).
    pub fn build(self) -> Result<Vec<u8>, tantivy_fst::Error> {
        // Build suffix FST.
        // Raw ordinals = position in sorted unique_tokens (same order as ._raw FST).
        let mut sfx_builder = SuffixFstBuilder::with_min_suffix_len(self.min_suffix_len);
        for (ordinal, token) in self.unique_tokens.iter().enumerate() {
            sfx_builder.add_token(token, ordinal as u64);
        }

        let num_terms = sfx_builder.num_terms() as u32;
        let (fst_data, parent_list_data) = sfx_builder.build()?;

        // Serialize gapmap
        let gapmap_data = self.gapmap_writer.serialize();

        // Assemble .sfx file
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

    #[test]
    fn test_collector_single_doc() {
        let mut collector = SfxCollector::new();

        // Simulate indexing: "import rag3db from 'rag3db_core';"
        collector.begin_doc();
        collector.add_token("import", 0, 6);
        collector.add_token("rag3db", 7, 13);
        collector.add_token("from", 14, 18);
        collector.add_token("rag3db", 20, 26);
        collector.add_token("core", 27, 31);
        collector.end_doc("import rag3db from 'rag3db_core';");

        let sfx_bytes = collector.build().unwrap();
        let reader = SfxFileReader::open(&sfx_bytes).unwrap();

        // Check suffix resolution
        let parents = reader.resolve_suffix("g3db");
        assert_eq!(parents.len(), 1);
        // "rag3db" is at some ordinal in sorted unique tokens
        assert_eq!(parents[0].si, 2); // SI=2 for "g3db" in "rag3db"

        // Check gapmap
        assert_eq!(reader.gapmap().num_tokens(0), 5);
        assert_eq!(reader.gapmap().read_gap(0, 0), b"");       // prefix
        assert_eq!(reader.gapmap().read_gap(0, 1), b" ");       // import<->rag3db
        assert_eq!(reader.gapmap().read_gap(0, 2), b" ");       // rag3db<->from
        assert_eq!(reader.gapmap().read_gap(0, 3), b" '");      // from<->rag3db
        assert_eq!(reader.gapmap().read_gap(0, 4), b"_");       // rag3db<->core
        assert_eq!(reader.gapmap().read_gap(0, 5), b"';");      // suffix
    }

    #[test]
    fn test_collector_multi_docs() {
        let mut collector = SfxCollector::new();

        // Doc 0
        collector.begin_doc();
        collector.add_token("hello", 0, 5);
        collector.add_token("world", 6, 11);
        collector.end_doc("hello world");

        // Doc 1
        collector.begin_doc();
        collector.add_token("foo", 0, 3);
        collector.end_doc("foo");

        // Doc 2 - empty
        collector.begin_doc();
        collector.end_doc_empty();

        let sfx_bytes = collector.build().unwrap();
        let reader = SfxFileReader::open(&sfx_bytes).unwrap();

        assert_eq!(reader.num_docs(), 3);
        assert_eq!(reader.gapmap().num_tokens(0), 2);
        assert_eq!(reader.gapmap().num_tokens(1), 1);
        assert_eq!(reader.gapmap().num_tokens(2), 0);

        // "orld" is suffix of "world"
        let parents = reader.resolve_suffix("orld");
        assert_eq!(parents.len(), 1);
        assert_eq!(parents[0].si, 1);
    }

    #[test]
    fn test_collector_prefix_walk() {
        let mut collector = SfxCollector::new();

        collector.begin_doc();
        collector.add_token("framework", 0, 9);
        collector.end_doc("framework");

        let sfx_bytes = collector.build().unwrap();
        let reader = SfxFileReader::open(&sfx_bytes).unwrap();

        // Prefix walk "work" should find "work" (suffix of "framework" at SI=5)
        let results = reader.prefix_walk("work");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "work");
        assert_eq!(results[0].1[0].si, 5);

        // Prefix walk "fram" should find "framework"
        let results = reader.prefix_walk("fram");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "framework");
        assert_eq!(results[0].1[0].si, 0);
    }

    #[test]
    fn test_collector_ordinals_match_sorted_tokens() {
        let mut collector = SfxCollector::new();

        // Tokens in insertion order: "zebra", "apple", "mango"
        collector.begin_doc();
        collector.add_token("zebra", 0, 5);
        collector.add_token("apple", 6, 11);
        collector.add_token("mango", 12, 17);
        collector.end_doc("zebra apple mango");

        let sfx_bytes = collector.build().unwrap();
        let reader = SfxFileReader::open(&sfx_bytes).unwrap();

        // Sorted order: apple(0), mango(1), zebra(2)
        let parents = reader.resolve_suffix("apple");
        assert_eq!(parents[0], ParentEntry { raw_ordinal: 0, si: 0 });

        let parents = reader.resolve_suffix("mango");
        assert_eq!(parents[0], ParentEntry { raw_ordinal: 1, si: 0 });

        let parents = reader.resolve_suffix("zebra");
        assert_eq!(parents[0], ParentEntry { raw_ordinal: 2, si: 0 });
    }
}
