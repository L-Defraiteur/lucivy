//! Suffix FST contains query — standalone query type for substring search via .sfx files.
//!
//! Unlike NgramContainsQuery which uses trigrams + stored text verification,
//! this query uses the suffix FST for direct proof. Zero stored text reads.
//!
//! Requires a .sfx file to exist for the target field. If not present,
//! returns an error — no silent fallback.

use std::sync::Arc;

use crate::docset::{DocSet, TERMINATED};
use crate::fieldnorm::FieldNormReader;
use crate::query::bm25::Bm25Weight;
use crate::query::phrase_query::scoring_utils::HighlightSink;
use crate::query::{EmptyScorer, EnableScoring, Explanation, Query, Scorer, Weight};
use crate::schema::Field;
use crate::suffix_fst::file::SfxFileReader;
use crate::{DocId, Score, SegmentReader};

use crate::tokenizer::{SimpleTokenizer, TextAnalyzer, LowerCaser, TokenStream, Tokenizer};

use super::suffix_contains;

/// Count term frequency per document from match doc_ids (already extracted).
/// Input must be sorted. Returns (doc_id, tf) pairs.
fn count_tf_sorted(doc_ids: &[DocId]) -> Vec<(DocId, u32)> {
    if doc_ids.is_empty() {
        return Vec::new();
    }
    let mut result = Vec::with_capacity(doc_ids.len() / 2 + 1);
    let mut prev = doc_ids[0];
    let mut count = 1u32;
    for &d in &doc_ids[1..] {
        if d == prev {
            count += 1;
        } else {
            result.push((prev, count));
            prev = d;
            count = 1;
        }
    }
    result.push((prev, count));
    result
}

/// Tokenize a query string into (tokens, separators) using the same
/// SimpleTokenizer + LowerCaser as the ._raw field.
/// Returns (["rust", "lang"], ["🦀"]) for "rust🦀lang".
/// Tokenize with the full RAW_TOKENIZER (SimpleTokenizer + CamelCaseSplit + LowerCaser).
/// Used for cross-token expansion: uppercase query → camelCase split → multi-token search.
fn tokenize_query_raw(query: &str) -> (Vec<String>, Vec<String>) {
    use crate::tokenizer::CamelCaseSplitFilter;
    let mut tokenizer = TextAnalyzer::builder(SimpleTokenizer::default())
        .filter(CamelCaseSplitFilter)
        .filter(LowerCaser)
        .build();
    let mut stream = tokenizer.token_stream(query);
    let mut tokens = Vec::new();
    let mut separators = Vec::new();
    let mut last_end: usize = 0;
    while stream.advance() {
        let tok = stream.token();
        if !tokens.is_empty() {
            let sep = &query[last_end..tok.offset_from];
            separators.push(sep.to_lowercase());
        }
        tokens.push(tok.text.clone());
        last_end = tok.offset_to;
    }
    (tokens, separators)
}

fn tokenize_query(query: &str) -> (Vec<String>, Vec<String>) {
    let mut tokenizer = TextAnalyzer::builder(SimpleTokenizer::default())
        .filter(LowerCaser)
        .build();
    let mut stream = tokenizer.token_stream(query);

    let mut tokens = Vec::new();
    let mut separators = Vec::new();
    let mut last_end: usize = 0;

    while stream.advance() {
        let tok = stream.token();
        if !tokens.is_empty() {
            // Separator = bytes between previous token end and this token start
            let sep = &query[last_end..tok.offset_from];
            separators.push(sep.to_lowercase());
        }
        tokens.push(tok.text.clone());
        last_end = tok.offset_to;
    }

    (tokens, separators)
}

/// A contains query backed by the suffix FST (.sfx file).
///
/// Supports single-token d=0 contains search. The .sfx file must exist
/// for the target `._raw` field.
#[derive(Debug, Clone)]
pub struct SuffixContainsQuery {
    raw_field: Field,
    query_text: String,
    fuzzy_distance: u8,
    /// If true, only match tokens that START with the query (SI=0 filter).
    prefix_only: bool,
    highlight_sink: Option<Arc<HighlightSink>>,
    highlight_field_name: String,
    /// If true, use continuation DFA to match across token boundaries.
    continuation: bool,
}

impl SuffixContainsQuery {
    /// Create a new suffix contains query.
    ///
    /// `raw_field` is the `._raw` field that has a corresponding .sfx file.
    /// `query_text` is the substring to search for.
    pub fn new(raw_field: Field, query_text: String) -> Self {
        Self {
            raw_field,
            query_text,
            fuzzy_distance: 0,
            prefix_only: false,
            highlight_sink: None,
            highlight_field_name: String::new(),
            continuation: false,
        }
    }

    /// Only match tokens that START with the query (prefix/startsWith mode).
    /// Filters suffix FST entries to SI=0 (full token start, not substring).
    pub fn with_prefix_only(mut self) -> Self {
        self.prefix_only = true;
        self
    }

    /// Enable cross-token continuation via DFA.
    /// Finds matches that span token boundaries (e.g., FUNCTION split into FUNC+TION).
    pub fn with_continuation(mut self, enabled: bool) -> Self {
        self.continuation = enabled;
        self
    }

    /// Set fuzzy Levenshtein distance (0 = exact).
    pub fn with_fuzzy_distance(mut self, distance: u8) -> Self {
        self.fuzzy_distance = distance;
        self
    }

    /// Attach a highlight sink for collecting byte offsets of matches.
    pub fn with_highlight_sink(mut self, sink: Arc<HighlightSink>, field_name: String) -> Self {
        self.highlight_sink = Some(sink);
        self.highlight_field_name = field_name;
        self
    }
}

impl Query for SuffixContainsQuery {
    fn weight(&self, enable_scoring: EnableScoring) -> crate::Result<Box<dyn Weight>> {
        let scoring_enabled = matches!(enable_scoring, EnableScoring::Enabled { .. });

        Ok(Box::new(SuffixContainsWeight {
            raw_field: self.raw_field,
            query_text: self.query_text.clone(),
            fuzzy_distance: self.fuzzy_distance,
            prefix_only: self.prefix_only,
            highlight_sink: self.highlight_sink.clone(),
            highlight_field_name: self.highlight_field_name.clone(),
            scoring_enabled,
            continuation: self.continuation,
        }))
    }
}

struct SuffixContainsWeight {
    raw_field: Field,
    query_text: String,
    fuzzy_distance: u8,
    prefix_only: bool,
    highlight_sink: Option<Arc<HighlightSink>>,
    highlight_field_name: String,
    scoring_enabled: bool,
    continuation: bool,
}

impl Weight for SuffixContainsWeight {
    fn scorer(&self, reader: &SegmentReader, boost: Score) -> crate::Result<Box<dyn Scorer>> {
        let segment_id = reader.segment_id();

        // Open the .sfx file — if not present (e.g. merged segment pending
        // sfx rebuild), return an empty scorer (no results from this segment).
        let sfx_data = match reader.sfx_file(self.raw_field) {
            Some(data) => data,
            None => return Ok(Box::new(crate::query::EmptyScorer)),
        };
        let sfx_bytes = sfx_data.read_bytes().map_err(|e| {
            crate::LucivyError::SystemError(format!("read .sfx: {e}"))
        })?;
        let sfx_reader = SfxFileReader::open(sfx_bytes.as_ref()).map_err(|e| {
            crate::LucivyError::SystemError(format!("open .sfx: {e}"))
        })?;

        let pr = crate::query::posting_resolver::build_resolver(reader, self.raw_field)?;
        // Adapt PostingResolver to the closure signature expected by suffix_contains functions
        let resolver = move |raw_ordinal: u64| -> Vec<suffix_contains::RawPostingEntry> {
            pr.resolve(raw_ordinal).into_iter().map(|e| suffix_contains::RawPostingEntry {
                doc_id: e.doc_id,
                token_index: e.position,
                byte_from: e.byte_from,
                byte_to: e.byte_to,
            }).collect()
        };

        // Tokenize the query to determine single vs multi-token path
        let (query_tokens, query_separators) = tokenize_query(&self.query_text);

        let fuzzy_d = self.fuzzy_distance;

        let prefix_only = self.prefix_only;

        let (doc_tf, highlights) = if query_tokens.len() <= 1 {
            // Single-token path
            let query = if query_tokens.is_empty() { &self.query_text } else { &query_tokens[0] };
            let mut matches = if fuzzy_d == 0 {
                if prefix_only {
                    suffix_contains::suffix_contains_single_token_prefix(
                        &sfx_reader, query, &resolver,
                    )
                } else if self.continuation {
                    suffix_contains::suffix_contains_single_token_continuation(
                        &sfx_reader, query, &resolver,
                    )
                } else {
                    suffix_contains::suffix_contains_single_token(
                        &sfx_reader, query, &resolver,
                    )
                }
            } else {
                if prefix_only {
                    suffix_contains::suffix_contains_single_token_fuzzy_prefix(
                        &sfx_reader, query, fuzzy_d, &resolver,
                    )
                } else {
                    suffix_contains::suffix_contains_single_token_fuzzy(
                        &sfx_reader, query, fuzzy_d, &resolver,
                    )
                }
            };

            // Cross-token expansion: tokenize the query in UPPERCASE with the
            // full RAW_TOKENIZER (CamelCaseSplit). If this produces multiple
            // tokens, run multi-token search with empty gap separators and
            // union the results. This catches cases where the tokenizer split
            // an ALL_CAPS word in the document (e.g., FUNCTION → FUNC+TION).
            if !prefix_only && fuzzy_d == 0 {
                let upper_query = self.query_text.to_uppercase();
                let (upper_tokens, _) = tokenize_query_raw(&upper_query);
                if upper_tokens.len() > 1 {
                    let token_refs: Vec<&str> = upper_tokens.iter().map(|s| s.as_str()).collect();
                    let empty_seps: Vec<&str> = vec![""; upper_tokens.len() - 1];
                    let multi_matches = suffix_contains::suffix_contains_multi_token(
                        &sfx_reader, &token_refs, &empty_seps, &resolver,
                    );
                    // Merge: add multi-token matches as single-token match entries
                    let existing_docs: std::collections::HashSet<u32> =
                        matches.iter().map(|m| m.doc_id).collect();
                    for mm in multi_matches {
                        if !existing_docs.contains(&mm.doc_id) {
                            matches.push(suffix_contains::SuffixContainsMatch {
                                doc_id: mm.doc_id,
                                token_index: mm.token_matches.first().map(|m| m.token_index).unwrap_or(0),
                                byte_from: mm.byte_from,
                                byte_to: mm.byte_to,
                                parent_term: String::new(),
                                si: 0,
                            });
                        }
                    }
                }
            }

            // Continuation is handled inside suffix_contains_single_token_inner
            // via the sfx_reader parameter (passed through the closure).

            // Emit diagnostic events
            {
                let bus = crate::diag::diag_bus();
                if bus.is_active() {
                    let seg_id = segment_id.uuid_string();
                    for m in &matches {
                        bus.emit(crate::diag::DiagEvent::SearchMatch {
                            query: self.query_text.clone(),
                            segment_id: seg_id.clone(),
                            doc_id: m.doc_id,
                            byte_from: m.byte_from,
                            byte_to: m.byte_to,
                            cross_token: false, // TODO: tag cross-token matches
                        });
                    }
                    let mut unique: Vec<DocId> = matches.iter().map(|m| m.doc_id).collect();
                    unique.sort_unstable();
                    unique.dedup();
                    bus.emit(crate::diag::DiagEvent::SearchComplete {
                        query: self.query_text.clone(),
                        segment_id: seg_id,
                        total_docs: unique.len() as u32,
                    });
                }
            }

            let highlights: Vec<(DocId, usize, usize)> = matches.iter()
                .map(|m| (m.doc_id, m.byte_from, m.byte_to))
                .collect();
            let mut doc_ids: Vec<DocId> = matches.iter().map(|m| m.doc_id).collect();
            doc_ids.sort_unstable();
            (count_tf_sorted(&doc_ids), highlights)
        } else {
            // Multi-token path
            let token_refs: Vec<&str> = query_tokens.iter().map(|s| s.as_str()).collect();
            let sep_refs: Vec<&str> = query_separators.iter().map(|s| s.as_str()).collect();
            let matches = if fuzzy_d == 0 {
                if prefix_only {
                    suffix_contains::suffix_contains_multi_token_prefix(
                        &sfx_reader, &token_refs, &sep_refs, resolver,
                    )
                } else {
                    suffix_contains::suffix_contains_multi_token(
                        &sfx_reader, &token_refs, &sep_refs, resolver,
                    )
                }
            } else {
                if prefix_only {
                    suffix_contains::suffix_contains_multi_token_fuzzy_prefix(
                        &sfx_reader, &token_refs, &sep_refs, resolver, fuzzy_d,
                    )
                } else {
                    suffix_contains::suffix_contains_multi_token_fuzzy(
                        &sfx_reader, &token_refs, &sep_refs, resolver, fuzzy_d,
                    )
                }
            };
            let highlights: Vec<(DocId, usize, usize)> = matches.iter()
                .map(|m| (m.doc_id, m.byte_from, m.byte_to))
                .collect();
            let mut doc_ids: Vec<DocId> = matches.iter().map(|m| m.doc_id).collect();
            doc_ids.sort_unstable();
            (count_tf_sorted(&doc_ids), highlights)
        };

        if doc_tf.is_empty() {
            return Ok(Box::new(EmptyScorer));
        }

        // Report highlights
        if let Some(ref sink) = self.highlight_sink {
            for &(doc_id, byte_from, byte_to) in &highlights {
                sink.insert(
                    segment_id,
                    doc_id,
                    &self.highlight_field_name,
                    vec![[byte_from, byte_to]],
                );
            }
        }

        let fieldnorm_reader = if let Some(fnr) = reader
            .fieldnorms_readers()
            .get_field(self.raw_field)?
        {
            fnr
        } else {
            FieldNormReader::constant(reader.max_doc(), 1)
        };

        // Compute BM25 per-segment from actual match statistics (no ._raw term dict needed)
        let bm25_weight = if self.scoring_enabled {
            let inverted_index = reader.inverted_index(self.raw_field)?;
            let total_num_tokens = inverted_index.total_num_tokens();
            let total_num_docs = (reader.max_doc() as u64).max(1);
            let average_fieldnorm = total_num_tokens as Score / total_num_docs as Score;
            Bm25Weight::for_one_term(doc_tf.len() as u64, total_num_docs, average_fieldnorm)
        } else {
            Bm25Weight::for_one_term(0, 1, 1.0)
        };

        Ok(Box::new(SuffixContainsScorer::new(
            doc_tf,
            bm25_weight.boost_by(boost),
            fieldnorm_reader,
        )))
    }

    fn explain(&self, reader: &SegmentReader, doc: DocId) -> crate::Result<Explanation> {
        let mut scorer = self.scorer(reader, 1.0)?;
        if scorer.seek(doc) != doc {
            return Err(crate::LucivyError::InvalidArgument(format!(
                "Document {doc} does not match"
            )));
        }
        Ok(Explanation::new("SuffixContainsScorer", scorer.score()))
    }
}

/// Scorer that iterates pre-verified doc IDs from the suffix FST.
/// Uses real term frequency (number of match positions per document) for BM25.
struct SuffixContainsScorer {
    /// (doc_id, term_freq) pairs, sorted by doc_id.
    candidates: Vec<(DocId, u32)>,
    cursor: usize,
    bm25_weight: Bm25Weight,
    fieldnorm_reader: FieldNormReader,
}

impl SuffixContainsScorer {
    fn new(
        candidates: Vec<(DocId, u32)>,
        bm25_weight: Bm25Weight,
        fieldnorm_reader: FieldNormReader,
    ) -> Self {
        Self { candidates, cursor: 0, bm25_weight, fieldnorm_reader }
    }
}

impl DocSet for SuffixContainsScorer {
    fn advance(&mut self) -> DocId {
        self.cursor += 1;
        self.doc()
    }

    fn doc(&self) -> DocId {
        if self.cursor < self.candidates.len() {
            self.candidates[self.cursor].0
        } else {
            TERMINATED
        }
    }

    fn size_hint(&self) -> u32 {
        self.candidates.len() as u32
    }

    fn seek(&mut self, target: DocId) -> DocId {
        while self.doc() < target {
            if self.advance() == TERMINATED {
                return TERMINATED;
            }
        }
        self.doc()
    }
}

impl Scorer for SuffixContainsScorer {
    fn score(&mut self) -> Score {
        if self.cursor >= self.candidates.len() { return 0.0; }
        let (doc, tf) = self.candidates[self.cursor];
        let fieldnorm_id = self.fieldnorm_reader.fieldnorm_id(doc);
        self.bm25_weight.score(fieldnorm_id, tf)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::query::phrase_query::scoring_utils::HighlightSink;
    use crate::query::EnableScoring;
    use crate::schema::{SchemaBuilder, IndexRecordOption, TextFieldIndexing, TextOptions};
    use crate::tokenizer::{LowerCaser, SimpleTokenizer, TextAnalyzer};
    use crate::{Index, LucivyDocument};

    /// Build an index with a `body._raw` field (which triggers .sfx generation).
    fn build_unicode_index() -> (Index, Field) {
        let mut schema_builder = SchemaBuilder::new();
        let raw_opts = TextOptions::default().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer("raw")
                .set_index_option(IndexRecordOption::WithFreqsAndPositionsAndOffsets),
        );
        let body_raw = schema_builder.add_text_field("body._raw", raw_opts);
        let schema = schema_builder.build();

        let index = Index::create_in_ram(schema);
        let raw_tokenizer = TextAnalyzer::builder(SimpleTokenizer::default())
            .filter(LowerCaser)
            .build();
        index.tokenizers().register("raw", raw_tokenizer);

        let mut writer = index.writer_for_tests().unwrap();

        // Doc 0: French accents
        let mut doc = LucivyDocument::new();
        doc.add_text(body_raw, "résumé café François");
        writer.add_document(doc).unwrap();

        // Doc 1: ASCII + substring match target
        let mut doc = LucivyDocument::new();
        doc.add_text(body_raw, "import rag3db from core");
        writer.add_document(doc).unwrap();

        // Doc 2: CJK
        let mut doc = LucivyDocument::new();
        doc.add_text(body_raw, "東京タワー hello 世界");
        writer.add_document(doc).unwrap();

        // Doc 3: emoji
        let mut doc = LucivyDocument::new();
        doc.add_text(body_raw, "rust🦀lang crème brûlée");
        writer.add_document(doc).unwrap();

        writer.commit().unwrap();

        (index, body_raw)
    }

    /// Helper: run SuffixContainsQuery and collect matching doc_ids.
    fn search_docs(index: &Index, field: Field, query_text: &str) -> Vec<DocId> {
        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        let query = SuffixContainsQuery::new(field, query_text.into());
        let weight = query.weight(EnableScoring::disabled_from_schema(searcher.schema())).unwrap();

        let mut all_docs = Vec::new();
        for seg_reader in searcher.segment_readers() {
            let mut scorer = weight.scorer(seg_reader, 1.0).unwrap();
            loop {
                let doc = scorer.doc();
                if doc == TERMINATED { break; }
                all_docs.push(doc);
                scorer.advance();
            }
        }
        all_docs
    }

    /// Helper: run SuffixContainsQuery with highlights.
    fn search_with_highlights(
        index: &Index,
        field: Field,
        query_text: &str,
        field_name: &str,
    ) -> (Vec<DocId>, Arc<HighlightSink>) {
        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        let sink = Arc::new(HighlightSink::new());
        let query = SuffixContainsQuery::new(field, query_text.into())
            .with_highlight_sink(Arc::clone(&sink), field_name.into());
        let weight = query.weight(EnableScoring::disabled_from_schema(searcher.schema())).unwrap();

        let mut all_docs = Vec::new();
        for seg_reader in searcher.segment_readers() {
            let mut scorer = weight.scorer(seg_reader, 1.0).unwrap();
            loop {
                let doc = scorer.doc();
                if doc == TERMINATED { break; }
                all_docs.push(doc);
                scorer.advance();
            }
        }
        (all_docs, sink)
    }

    #[test]
    fn test_suffix_query_exact_ascii() {
        let (index, body_raw) = build_unicode_index();
        let docs = search_docs(&index, body_raw, "rag3db");
        assert_eq!(docs, vec![1]);
    }

    #[test]
    fn test_suffix_query_substring() {
        let (index, body_raw) = build_unicode_index();
        // "g3db" is a suffix of "rag3db"
        let docs = search_docs(&index, body_raw, "g3db");
        assert_eq!(docs, vec![1]);
    }

    #[test]
    fn test_suffix_query_french_accents() {
        let (index, body_raw) = build_unicode_index();
        let docs = search_docs(&index, body_raw, "café");
        assert_eq!(docs, vec![0]);
    }

    #[test]
    fn test_suffix_query_cjk() {
        let (index, body_raw) = build_unicode_index();
        let docs = search_docs(&index, body_raw, "世界");
        assert_eq!(docs, vec![2]);
    }

    #[test]
    fn test_suffix_query_emoji() {
        let (index, body_raw) = build_unicode_index();
        // "rust🦀lang" is one token (SimpleTokenizer splits on whitespace)
        let docs = search_docs(&index, body_raw, "rust🦀lang");
        assert_eq!(docs, vec![3]);
    }

    #[test]
    fn test_suffix_query_no_match() {
        let (index, body_raw) = build_unicode_index();
        let docs = search_docs(&index, body_raw, "nonexistent");
        assert!(docs.is_empty());
    }

    #[test]
    fn test_suffix_query_highlights_cafe() {
        let (index, body_raw) = build_unicode_index();
        let (docs, sink) = search_with_highlights(&index, body_raw, "café", "body");
        assert_eq!(docs, vec![0]);

        let reader = index.reader().unwrap();
        let seg_id = reader.searcher().segment_readers()[0].segment_id();
        let highlights = sink.get(seg_id, 0).expect("highlights for doc 0");
        let offsets = highlights.get("body").expect("body highlights");
        assert_eq!(offsets.len(), 1);
        // "résumé " = 8+1 = 9 bytes, "café" = 5 bytes
        assert_eq!(offsets[0], [9, 14]);
    }

    #[test]
    fn test_suffix_query_highlights_substring_unicode() {
        let (index, body_raw) = build_unicode_index();
        // "afé" is suffix of "café" at SI=1 byte
        let (docs, sink) = search_with_highlights(&index, body_raw, "afé", "body");
        assert_eq!(docs, vec![0]);

        let reader = index.reader().unwrap();
        let seg_id = reader.searcher().segment_readers()[0].segment_id();
        let highlights = sink.get(seg_id, 0).expect("highlights for doc 0");
        let offsets = highlights.get("body").expect("body highlights");
        assert_eq!(offsets.len(), 1);
        // "café" at byte 9, "afé" at SI=1 → byte 10, length=4
        assert_eq!(offsets[0], [10, 14]);
    }

    #[test]
    fn test_suffix_query_highlights_brûlée() {
        let (index, body_raw) = build_unicode_index();
        let (docs, sink) = search_with_highlights(&index, body_raw, "brûlée", "body");
        assert_eq!(docs, vec![3]);

        let reader = index.reader().unwrap();
        let seg_id = reader.searcher().segment_readers()[0].segment_id();
        let highlights = sink.get(seg_id, 3).expect("highlights for doc 3");
        let offsets = highlights.get("body").expect("body highlights");
        assert_eq!(offsets.len(), 1);
        // "rust🦀lang"=12, " "=1, "crème"=6, " "=1 → 20
        // "brûlée" = 8 bytes (b=1,r=1,û=2,l=1,é=2,e=1)
        assert_eq!(offsets[0], [20, 28]);
    }
}
