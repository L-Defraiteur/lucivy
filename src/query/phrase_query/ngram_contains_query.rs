//! N-gram based contains query for fast substring search.
//!
//! Uses a `._ngram` field (trigrams) to quickly identify candidate documents,
//! then verifies matches by reading stored text. Replaces the expensive FST walk
//! (levels 3–4 of the cascade) with O(k lookups + candidates) via trigram index.
//!
//! Cascade:
//!   1. **Exact**: term dict lookup on raw field — O(1)
//!   2. **Ngram**: trigram lookup on ngram field + verification — O(k + candidates)
//!
//! Verification mode:
//!   - **Fuzzy**: token-by-token matching with Levenshtein distance (default)
//!   - **Regex**: compiled regex on stored text, with optional fuzzy on extracted literals

use std::cmp::{max, min};
use std::sync::Arc;

use regex::Regex;

use super::scoring_utils::{
    edit_distance, generate_trigrams, intersect_sorted_vecs, ngram_threshold,
    token_match_distance, tokenize_raw, HighlightSink,
};
use crate::fieldnorm::FieldNormReader;
use crate::query::bm25::Bm25Weight;
use crate::query::{EmptyScorer, EnableScoring, Explanation, Query, Scorer, Weight};
use crate::schema::document::Value;
use crate::schema::{Field, IndexRecordOption, Term};
use crate::index::SegmentId;
use crate::{DocId, DocSet, InvertedIndexReader, Score, SegmentReader, LucivyDocument, TERMINATED};

// ─── Candidate Collection ──────────────────────────────────────────────────

/// Collect doc_ids from a single term's posting list.
fn collect_posting_docs(
    inverted_index: &InvertedIndexReader,
    term: &Term,
) -> crate::Result<Vec<DocId>> {
    let term_info = match inverted_index.get_term_info(term)? {
        Some(ti) => ti,
        None => return Ok(Vec::new()),
    };
    let mut docs = Vec::new();
    let mut block_postings =
        inverted_index.read_block_postings_from_terminfo(&term_info, IndexRecordOption::Basic)?;
    loop {
        let block = block_postings.docs();
        if block.is_empty() {
            break;
        }
        docs.extend_from_slice(block);
        block_postings.advance();
    }
    Ok(docs)
}

/// Get candidate doc_ids for a query token via threshold-based trigram intersection.
fn ngram_candidates_for_token(
    token: &str,
    ngram_field: Field,
    ngram_inverted: &InvertedIndexReader,
    fuzzy_distance: u8,
) -> crate::Result<Vec<DocId>> {
    let trigrams = generate_trigrams(token);
    let threshold = ngram_threshold(trigrams.len(), fuzzy_distance);

    if trigrams.is_empty() {
        return Ok(Vec::new());
    }

    // Collect all doc_ids from all trigram posting lists.
    let mut all_docs: Vec<DocId> = Vec::new();
    for trigram in &trigrams {
        let term = Term::from_field_text(ngram_field, trigram);
        let docs = collect_posting_docs(ngram_inverted, &term)?;
        all_docs.extend(docs);
    }

    // Sort and count: keep doc_ids appearing >= threshold times.
    all_docs.sort_unstable();

    let mut candidates = Vec::new();
    let mut i = 0;
    while i < all_docs.len() {
        let doc = all_docs[i];
        let mut count = 0usize;
        while i < all_docs.len() && all_docs[i] == doc {
            count += 1;
            i += 1;
        }
        if count >= threshold {
            candidates.push(doc);
        }
    }

    Ok(candidates)
}

// ─── Verification Mode ──────────────────────────────────────────────────────

/// Parameters for fuzzy substring verification.
#[derive(Clone, Debug)]
#[allow(missing_docs)]
pub struct FuzzyParams {
    pub tokens: Vec<String>,
    pub separators: Vec<String>,
    pub prefix: String,
    pub suffix: String,
    pub fuzzy_distance: u8,
    pub distance_budget: u32,
    pub strict_separators: bool,
}

/// Parameters for regex verification (with optional fuzzy on extracted literals).
#[derive(Clone, Debug)]
pub struct RegexParams {
    /// Compiled regex pattern for verification on stored text.
    pub compiled: Regex,
    /// Literals extracted from the regex AST (for hybrid fuzzy + trigram generation).
    pub literals: Vec<String>,
    /// Fuzzy distance for hybrid verification: 0 = regex only, >0 = regex OR fuzzy on literals.
    pub fuzzy_distance: u8,
}

/// Verification mode for NgramContainsQuery.
///
/// Determines how candidate documents are verified after trigram-based
/// candidate collection.
#[derive(Clone, Debug)]
pub enum VerificationMode {
    /// Fuzzy substring verification: token-by-token matching with
    /// Levenshtein distance, separator validation, and prefix/suffix checks.
    Fuzzy(FuzzyParams),
    /// Regex verification on stored text, with optional hybrid fuzzy on extracted literals.
    Regex(RegexParams),
}

// ─── Query ─────────────────────────────────────────────────────────────────

/// N-gram based contains query: trigram lookup + stored text verification.
///
/// Uses a trigram field to quickly narrow candidates, then verifies matches
/// by reading stored text. This avoids expensive FST walks for substring matching.
#[derive(Clone, Debug)]
pub struct NgramContainsQuery {
    raw_field: Field,
    ngram_field: Field,
    stored_field: Option<Field>,
    trigram_sources: Vec<String>,
    verification: VerificationMode,
    highlight_sink: Option<Arc<HighlightSink>>,
    highlight_field_name: String,
}

impl NgramContainsQuery {
    /// Creates a new `NgramContainsQuery`.
    ///
    /// * `raw_field` - Lowercase raw field for exact term lookups and BM25 stats.
    /// * `ngram_field` - Trigram field for candidate collection.
    /// * `stored_field` - Field to load stored text from (if different from `raw_field`).
    /// * `trigram_sources` - Strings used for trigram generation (tokens in fuzzy mode,
    ///   extracted literals in regex mode).
    /// * `verification` - How to verify candidates (fuzzy or regex).
    pub fn new(
        raw_field: Field,
        ngram_field: Field,
        stored_field: Option<Field>,
        trigram_sources: Vec<String>,
        verification: VerificationMode,
    ) -> Self {
        NgramContainsQuery {
            raw_field,
            ngram_field,
            stored_field,
            trigram_sources,
            verification,
            highlight_sink: None,
            highlight_field_name: String::new(),
        }
    }

    /// Attach a highlight sink to capture byte offsets during scoring.
    pub fn with_highlight_sink(mut self, sink: Arc<HighlightSink>, field_name: String) -> Self {
        self.highlight_sink = Some(sink);
        self.highlight_field_name = field_name;
        self
    }
}

impl Query for NgramContainsQuery {
    fn weight(&self, enable_scoring: EnableScoring) -> crate::Result<Box<dyn Weight>> {
        let bm25_weight = match enable_scoring {
            EnableScoring::Enabled {
                statistics_provider,
                ..
            } => {
                // Use trigram_sources as reference terms for BM25 stats.
                // In fuzzy mode these are the query tokens; in regex mode
                // they will be the extracted literals.
                let terms: Vec<Term> = self
                    .trigram_sources
                    .iter()
                    .map(|t| Term::from_field_text(self.raw_field, t))
                    .collect();
                if terms.is_empty() {
                    Bm25Weight::for_one_term(0, 1, 1.0)
                } else {
                    Bm25Weight::for_terms(statistics_provider, &terms)?
                }
            }
            EnableScoring::Disabled { .. } => Bm25Weight::for_one_term(0, 1, 1.0),
        };

        Ok(Box::new(NgramContainsWeight {
            raw_field: self.raw_field,
            ngram_field: self.ngram_field,
            stored_field: self.stored_field,
            trigram_sources: self.trigram_sources.clone(),
            verification: self.verification.clone(),
            highlight_sink: self.highlight_sink.clone(),
            highlight_field_name: self.highlight_field_name.clone(),
            bm25_weight,
        }))
    }
}

// ─── Weight ────────────────────────────────────────────────────────────────

struct NgramContainsWeight {
    raw_field: Field,
    ngram_field: Field,
    stored_field: Option<Field>,
    trigram_sources: Vec<String>,
    verification: VerificationMode,
    highlight_sink: Option<Arc<HighlightSink>>,
    highlight_field_name: String,
    bm25_weight: Bm25Weight,
}

impl Weight for NgramContainsWeight {
    fn scorer(&self, reader: &SegmentReader, boost: Score) -> crate::Result<Box<dyn Scorer>> {
        let segment_id = reader.segment_id();
        let raw_inverted = reader.inverted_index(self.raw_field)?;
        let ngram_inverted = reader.inverted_index(self.ngram_field)?;

        let final_candidates = match &self.verification {
            VerificationMode::Fuzzy(params) => {
                // Fuzzy mode: exact lookup first, then ngram, intersect across tokens.
                let mut per_token_candidates: Vec<Vec<DocId>> = Vec::new();
                for source in &self.trigram_sources {
                    let term = Term::from_field_text(self.raw_field, source);
                    let exact_docs = collect_posting_docs(&raw_inverted, &term)?;
                    if !exact_docs.is_empty() {
                        per_token_candidates.push(exact_docs);
                        continue;
                    }
                    let candidates = ngram_candidates_for_token(
                        source,
                        self.ngram_field,
                        &ngram_inverted,
                        params.fuzzy_distance,
                    )?;
                    per_token_candidates.push(candidates);
                }
                intersect_sorted_vecs(per_token_candidates)
            }
            VerificationMode::Regex(params) => {
                if self.trigram_sources.is_empty() {
                    // No usable trigrams (literals < 3 chars): full segment scan.
                    // All docs are candidates; regex verification will filter.
                    (0..reader.max_doc()).collect()
                } else {
                    // Ngram lookup: union across literals (alternatives).
                    let mut all_candidates: Vec<DocId> = Vec::new();
                    for source in &self.trigram_sources {
                        let candidates = ngram_candidates_for_token(
                            source,
                            self.ngram_field,
                            &ngram_inverted,
                            params.fuzzy_distance,
                        )?;
                        all_candidates.extend(candidates);
                    }
                    all_candidates.sort_unstable();
                    all_candidates.dedup();
                    all_candidates
                }
            }
        };

        if final_candidates.is_empty() {
            return Ok(Box::new(EmptyScorer));
        }

        // Create scorer that verifies each candidate via stored text.
        let store_reader = reader
            .get_store_reader(50)
            .map_err(crate::LucivyError::from)?;
        let text_field = self.stored_field.unwrap_or(self.raw_field);

        let fieldnorm_reader = if let Some(fnr) = reader
            .fieldnorms_readers()
            .get_field(self.raw_field)?
        {
            fnr
        } else {
            FieldNormReader::constant(reader.max_doc(), 1)
        };

        Ok(Box::new(NgramContainsScorer::new(
            final_candidates,
            store_reader,
            text_field,
            self.verification.clone(),
            self.bm25_weight.boost_by(boost),
            fieldnorm_reader,
            self.highlight_sink.clone(),
            self.highlight_field_name.clone(),
            segment_id,
        )))
    }

    fn explain(&self, reader: &SegmentReader, doc: DocId) -> crate::Result<Explanation> {
        let mut scorer = self.scorer(reader, 1.0)?;
        if scorer.seek(doc) != doc {
            return Err(crate::LucivyError::InvalidArgument(format!(
                "Document {doc} does not match"
            )));
        }
        Ok(Explanation::new("NgramContainsScorer", scorer.score()))
    }
}

// ─── Fuzzy Verification ─────────────────────────────────────────────────────

/// Count all matching positions for a single-token fuzzy query.
fn count_single_token_fuzzy(
    stored_text: &str,
    doc_tokens: &[(usize, usize)],
    params: &FuzzyParams,
    highlight_sink: &Option<Arc<HighlightSink>>,
    highlight_field_name: &str,
    segment_id: SegmentId,
    doc_id: DocId,
) -> u32 {
    let query_token = &params.tokens[0];
    let mut count = 0u32;

    for &(start, end) in doc_tokens {
        let doc_token = stored_text[start..end].to_lowercase();

        let distance = match token_match_distance(&doc_token, query_token, params.fuzzy_distance) {
            Some(d) => {
                d
            },
            None => {
                continue;
            },
        };

        let mut total_distance = distance;

        // Validate prefix.
        if !params.prefix.is_empty() {
            if params.strict_separators {
                let prefix_len = params.prefix.len();
                let doc_prefix_start = start.saturating_sub(prefix_len);
                let doc_prefix = &stored_text[doc_prefix_start..start];
                total_distance += edit_distance(&params.prefix, doc_prefix);
                if total_distance > params.distance_budget {
                    continue;
                }
            } else {
                if start == 0 {
                    continue;
                }
                if stored_text.as_bytes()[start - 1].is_ascii_alphanumeric() {
                    continue;
                }
            }
        }

        // Validate suffix.
        if !params.suffix.is_empty() {
            if params.strict_separators {
                let suffix_len = params.suffix.len();
                let doc_suffix_end = min(end + suffix_len, stored_text.len());
                let doc_suffix = &stored_text[end..doc_suffix_end];
                total_distance += edit_distance(&params.suffix, doc_suffix);
                if total_distance > params.distance_budget {
                    continue;
                }
            } else {
                if end >= stored_text.len() {
                    continue;
                }
                if stored_text.as_bytes()[end].is_ascii_alphanumeric() {
                    continue;
                }
            }
        }

        count += 1;
        if let Some(sink) = highlight_sink {
            sink.insert(segment_id, doc_id, highlight_field_name, vec![[start, end]]);
        }
    }
    count
}

/// Count all matching positions for a multi-token fuzzy query.
fn count_multi_token_fuzzy(
    stored_text: &str,
    doc_tokens: &[(usize, usize)],
    params: &FuzzyParams,
    highlight_sink: &Option<Arc<HighlightSink>>,
    highlight_field_name: &str,
    segment_id: SegmentId,
    doc_id: DocId,
) -> u32 {
    let num_query = params.tokens.len();
    if doc_tokens.len() < num_query {
        return 0;
    }

    let mut count = 0u32;
    for start_idx in 0..=(doc_tokens.len() - num_query) {
        if check_at_position_fuzzy(
            stored_text,
            doc_tokens,
            start_idx,
            params,
            highlight_sink,
            highlight_field_name,
            segment_id,
            doc_id,
        ) {
            count += 1;
        }
    }
    count
}

/// Check if a multi-token fuzzy query matches at a specific position.
fn check_at_position_fuzzy(
    stored_text: &str,
    doc_tokens: &[(usize, usize)],
    start_idx: usize,
    params: &FuzzyParams,
    highlight_sink: &Option<Arc<HighlightSink>>,
    highlight_field_name: &str,
    segment_id: SegmentId,
    doc_id: DocId,
) -> bool {
    let mut total_distance = 0u32;

    // Check each query token matches the corresponding doc token.
    for (q_idx, query_token) in params.tokens.iter().enumerate() {
        let (start, end) = doc_tokens[start_idx + q_idx];
        let doc_token = stored_text[start..end].to_lowercase();

        match token_match_distance(&doc_token, query_token, params.fuzzy_distance) {
            Some(d) => total_distance += d,
            None => return false,
        }

        if total_distance > params.distance_budget {
            return false;
        }
    }

    // Validate separators between consecutive tokens.
    for (sep_idx, query_sep) in params.separators.iter().enumerate() {
        let (_, end_i) = doc_tokens[start_idx + sep_idx];
        let (start_next, _) = doc_tokens[start_idx + sep_idx + 1];
        if end_i > stored_text.len() || start_next > stored_text.len() || end_i > start_next {
            return false;
        }
        let doc_sep = &stored_text[end_i..start_next];
        if params.strict_separators {
            total_distance += edit_distance(query_sep, doc_sep);
            if total_distance > params.distance_budget {
                return false;
            }
        } else if doc_sep.is_empty() || !doc_sep.bytes().any(|b| !b.is_ascii_alphanumeric()) {
            return false;
        }
    }

    // Validate prefix.
    if !params.prefix.is_empty() {
        let (first_start, _) = doc_tokens[start_idx];
        if params.strict_separators {
            let prefix_len = params.prefix.len();
            let doc_prefix_start = first_start.saturating_sub(prefix_len);
            let doc_prefix = &stored_text[doc_prefix_start..first_start];
            total_distance += edit_distance(&params.prefix, doc_prefix);
            if total_distance > params.distance_budget {
                return false;
            }
        } else {
            if first_start == 0 {
                return false;
            }
            let before = &stored_text[..first_start];
            if before
                .as_bytes()
                .last()
                .is_none_or(|b| b.is_ascii_alphanumeric())
            {
                return false;
            }
        }
    }

    // Validate suffix.
    if !params.suffix.is_empty() {
        let num_query = params.tokens.len();
        let (_, last_end) = doc_tokens[start_idx + num_query - 1];
        if params.strict_separators {
            let suffix_len = params.suffix.len();
            let doc_suffix_end = min(last_end + suffix_len, stored_text.len());
            let doc_suffix = &stored_text[last_end..doc_suffix_end];
            total_distance += edit_distance(&params.suffix, doc_suffix);
            if total_distance > params.distance_budget {
                return false;
            }
        } else {
            if last_end >= stored_text.len() {
                return false;
            }
            if stored_text.as_bytes()[last_end].is_ascii_alphanumeric() {
                return false;
            }
        }
    }

    if let Some(sink) = highlight_sink {
        let offsets: Vec<[usize; 2]> = (0..params.tokens.len())
            .map(|i| {
                let (s, e) = doc_tokens[start_idx + i];
                [s, e]
            })
            .collect();
        sink.insert(segment_id, doc_id, highlight_field_name, offsets);
    }
    true
}

// ─── Regex Verification ──────────────────────────────────────────────────────

/// Verify a candidate document using regex (pure or hybrid with fuzzy on literals).
///
/// - Pure regex (`fuzzy_distance == 0`): run `compiled.find_iter()` on stored text.
/// - Hybrid (`fuzzy_distance > 0`): also try fuzzy matching on extracted literals;
///   `tf = max(tf_regex, tf_fuzzy)`.
fn verify_regex(
    stored_text: &str,
    params: &RegexParams,
    highlight_sink: &Option<Arc<HighlightSink>>,
    highlight_field_name: &str,
    segment_id: SegmentId,
    doc_id: DocId,
) -> u32 {
    // 1. Regex exact verification
    let regex_matches: Vec<regex::Match> = params.compiled.find_iter(stored_text).collect();
    let tf_regex = regex_matches.len() as u32;

    // 2. Hybrid fuzzy verification on extracted literals (if distance > 0)
    let tf_fuzzy = if params.fuzzy_distance > 0 && !params.literals.is_empty() {
        let doc_tokens = tokenize_raw(stored_text);
        // For each literal, count fuzzy matches in the document tokens.
        // Use the max across all literals (each literal is an alternative).
        params
            .literals
            .iter()
            .map(|lit| {
                let lit_lower = lit.to_lowercase();
                let mut count = 0u32;
                for &(start, end) in &doc_tokens {
                    let doc_token = stored_text[start..end].to_lowercase();
                    if token_match_distance(&doc_token, &lit_lower, params.fuzzy_distance).is_some()
                    {
                        count += 1;
                    }
                }
                count
            })
            .max()
            .unwrap_or(0)
    } else {
        0
    };

    let tf = max(tf_regex, tf_fuzzy);

    // Highlights: prefer regex offsets if available, otherwise no offsets for fuzzy-only matches.
    if tf > 0 {
        if let Some(sink) = highlight_sink {
            if tf_regex > 0 {
                let offsets: Vec<[usize; 2]> = regex_matches
                    .iter()
                    .map(|m| [m.start(), m.end()])
                    .collect();
                sink.insert(segment_id, doc_id, highlight_field_name, offsets);
            }
            // When only fuzzy matched (tf_regex == 0), we don't have precise byte offsets.
        }
    }

    tf
}

// ─── Scorer ────────────────────────────────────────────────────────────────

struct NgramContainsScorer {
    candidates: Vec<DocId>,
    cursor: usize,
    store_reader: crate::store::StoreReader,
    text_field: Field,
    verification: VerificationMode,
    bm25_weight: Bm25Weight,
    fieldnorm_reader: FieldNormReader,
    last_tf: u32,
    highlight_sink: Option<Arc<HighlightSink>>,
    highlight_field_name: String,
    segment_id: SegmentId,
}

impl NgramContainsScorer {
    fn new(
        candidates: Vec<DocId>,
        store_reader: crate::store::StoreReader,
        text_field: Field,
        verification: VerificationMode,
        bm25_weight: Bm25Weight,
        fieldnorm_reader: FieldNormReader,
        highlight_sink: Option<Arc<HighlightSink>>,
        highlight_field_name: String,
        segment_id: SegmentId,
    ) -> Self {
        let mut scorer = NgramContainsScorer {
            candidates,
            cursor: 0,
            store_reader,
            text_field,
            verification,
            bm25_weight,
            fieldnorm_reader,
            last_tf: 0,
            highlight_sink,
            highlight_field_name,
            segment_id,
        };
        // Advance to first valid doc.
        if scorer.doc() != TERMINATED && !scorer.verify() {
            scorer.advance();
        }
        scorer
    }

    /// Verify the current candidate doc by reading stored text.
    /// Dispatches to the appropriate verification mode.
    fn verify(&mut self) -> bool {
        self.last_tf = 0;
        let doc_id = self.doc();
        if doc_id == TERMINATED {
            return false;
        }

        let doc: LucivyDocument = match self.store_reader.get(doc_id) {
            Ok(d) => d,
            Err(_) => return false,
        };
        let stored_text = match doc.get_first(self.text_field).and_then(|v| v.as_str()) {
            Some(s) => s,
            None => {
                return false;
            }
        };

        let tf = match &self.verification {
            VerificationMode::Fuzzy(params) => {
                let doc_tokens = tokenize_raw(stored_text);
                if params.tokens.len() == 1 {
                    count_single_token_fuzzy(
                        stored_text,
                        &doc_tokens,
                        params,
                        &self.highlight_sink,
                        &self.highlight_field_name,
                        self.segment_id,
                        doc_id,
                    )
                } else {
                    count_multi_token_fuzzy(
                        stored_text,
                        &doc_tokens,
                        params,
                        &self.highlight_sink,
                        &self.highlight_field_name,
                        self.segment_id,
                        doc_id,
                    )
                }
            }
            VerificationMode::Regex(params) => {
                verify_regex(
                    stored_text,
                    params,
                    &self.highlight_sink,
                    &self.highlight_field_name,
                    self.segment_id,
                    doc_id,
                )
            }
        };
        self.last_tf = tf;
        tf > 0
    }
}

impl DocSet for NgramContainsScorer {
    fn advance(&mut self) -> DocId {
        loop {
            self.cursor += 1;
            let doc = self.doc();
            if doc == TERMINATED || self.verify() {
                return doc;
            }
        }
    }

    fn seek(&mut self, target: DocId) -> DocId {
        while self.cursor < self.candidates.len() && self.candidates[self.cursor] < target {
            self.cursor += 1;
        }
        if self.doc() == TERMINATED || self.verify() {
            return self.doc();
        }
        let result = self.advance();
        result
    }

    fn doc(&self) -> DocId {
        if self.cursor < self.candidates.len() {
            self.candidates[self.cursor]
        } else {
            TERMINATED
        }
    }

    fn size_hint(&self) -> u32 {
        self.candidates.len().saturating_sub(self.cursor) as u32
    }
}

impl Scorer for NgramContainsScorer {
    fn score(&mut self) -> Score {
        let doc = self.doc();
        let fieldnorm_id = self.fieldnorm_reader.fieldnorm_id(doc);
        self.bm25_weight.score(fieldnorm_id, self.last_tf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::SegmentId;

    /// Dummy SegmentId for unit tests (all functions now require SegmentId instead of u32).
    fn test_seg_id() -> SegmentId {
        SegmentId::generate_random()
    }

    fn make_regex_params(pattern: &str, literals: Vec<&str>, fuzzy_distance: u8) -> RegexParams {
        RegexParams {
            compiled: Regex::new(&format!("(?i){pattern}")).unwrap(),
            literals: literals.into_iter().map(|s| s.to_string()).collect(),
            fuzzy_distance,
        }
    }

    // ─── verify_regex: pure regex ────────────────────────────────────────

    #[test]
    fn test_regex_pure_match() {
        let params = make_regex_params(r"program[a-z]+", vec!["program"], 0);
        let tf = verify_regex("Rust is a systems programming language", &params, &None, "", test_seg_id(), 0);
        assert_eq!(tf, 1); // "programming" matches
    }

    #[test]
    fn test_regex_pure_no_match() {
        let params = make_regex_params(r"program[a-z]+", vec!["program"], 0);
        let tf = verify_regex("the cat sat on the mat", &params, &None, "", test_seg_id(), 0);
        assert_eq!(tf, 0);
    }

    #[test]
    fn test_regex_pure_multiple_matches() {
        let params = make_regex_params(r"program[a-z]+", vec!["program"], 0);
        let tf = verify_regex(
            "Programming in Rust: a programmer's guide to programming",
            &params,
            &None,
            "",
            test_seg_id(),
            0,
        );
        assert_eq!(tf, 3); // "Programming", "programmer", "programming"
    }

    #[test]
    fn test_regex_case_insensitive() {
        let params = make_regex_params(r"rust", vec!["rust"], 0);
        let tf = verify_regex("Rust is great", &params, &None, "", test_seg_id(), 0);
        assert_eq!(tf, 1);
    }

    // ─── verify_regex: hybrid (regex + fuzzy) ────────────────────────────

    #[test]
    fn test_regex_hybrid_typo_in_pattern() {
        // Pattern has typo "programing" (one m) — regex won't match "programming"
        // but fuzzy on literal "programing" with distance=1 should match.
        let params = make_regex_params(r"programing[a-z]+", vec!["programing"], 1);
        let tf = verify_regex("Rust is a systems programming language", &params, &None, "", test_seg_id(), 0);
        assert!(tf > 0, "hybrid should match via fuzzy on literal");
    }

    #[test]
    fn test_regex_hybrid_exact_wins() {
        // Pattern is correct — regex matches directly, fuzzy also matches.
        // tf = max(regex, fuzzy).
        let params = make_regex_params(r"program[a-z]+", vec!["program"], 1);
        let tf = verify_regex("Rust programming is fun", &params, &None, "", test_seg_id(), 0);
        assert!(tf > 0);
    }

    #[test]
    fn test_regex_hybrid_no_match() {
        let params = make_regex_params(r"python[a-z]+", vec!["python"], 1);
        let tf = verify_regex("Rust is a systems programming language", &params, &None, "", test_seg_id(), 0);
        assert_eq!(tf, 0);
    }

    // ─── verify_regex: highlights ────────────────────────────────────────

    #[test]
    fn test_regex_highlights() {
        let sink = Arc::new(HighlightSink::new());
        let sid = test_seg_id();
        let params = make_regex_params(r"program[a-z]+", vec!["program"], 0);
        let text = "Rust programming is fun";
        let tf = verify_regex(text, &params, &Some(sink.clone()), "", sid, 42);
        assert_eq!(tf, 1);
        let by_field = sink.get(sid, 42).expect("should have highlights");
        let offsets = by_field.get("").expect("should have field offsets");
        assert_eq!(offsets.len(), 1);
        // "programming" starts at index 5 in "Rust programming is fun"
        assert_eq!(offsets[0], [5, 16]);
    }

    // ─── verify_regex: edge cases ────────────────────────────────────────

    #[test]
    fn test_regex_empty_text() {
        let params = make_regex_params(r"program[a-z]+", vec!["program"], 0);
        let tf = verify_regex("", &params, &None, "", test_seg_id(), 0);
        assert_eq!(tf, 0);
    }

    #[test]
    fn test_regex_dot_star() {
        let params = make_regex_params(r".*", vec![], 0);
        let tf = verify_regex("anything", &params, &None, "", test_seg_id(), 0);
        assert!(tf > 0); // .* matches everything
    }

    #[test]
    fn test_regex_word_boundary() {
        let params = make_regex_params(r"\brust\b", vec!["rust"], 0);
        let tf = verify_regex("Rust is great but rusty is not", &params, &None, "", test_seg_id(), 0);
        assert_eq!(tf, 1); // "Rust" matches, "rusty" does not
    }

    #[test]
    fn test_regex_unicode() {
        let params = make_regex_params(r"café", vec!["café"], 0);
        let tf = verify_regex("I love café au lait", &params, &None, "", test_seg_id(), 0);
        assert_eq!(tf, 1);
    }

    #[test]
    fn test_regex_hybrid_fuzzy_only_match() {
        // Regex "xyz[0-9]+" won't match any text, but fuzzy on literal "database"
        // with distance=1 should match "databse" (typo).
        let params = make_regex_params(r"databse", vec!["databse"], 1);
        let tf = verify_regex("Graph databases store data", &params, &None, "", test_seg_id(), 0);
        // "databse" is distance 1 from "database" (substring of "databases")
        assert!(tf > 0, "hybrid should match via fuzzy on literal");
    }

    #[test]
    fn test_regex_multiple_highlights() {
        let sink = Arc::new(HighlightSink::new());
        let sid = test_seg_id();
        let params = make_regex_params(r"[a-z]+ing", vec!["ing"], 0);
        let text = "programming and testing are fun";
        let tf = verify_regex(text, &params, &Some(sink.clone()), "", sid, 99);
        assert_eq!(tf, 2); // "programming" and "testing"
        let by_field = sink.get(sid, 99).expect("should have highlights");
        let offsets = by_field.get("").expect("should have field offsets");
        assert_eq!(offsets.len(), 2);
    }

    // ─── count_single_token_fuzzy ──────────────────────────────────────

    fn make_fuzzy_params(
        tokens: Vec<&str>,
        separators: Vec<&str>,
        prefix: &str,
        suffix: &str,
        distance: u8,
        budget: u32,
    ) -> FuzzyParams {
        FuzzyParams {
            tokens: tokens.into_iter().map(|s| s.to_string()).collect(),
            separators: separators.into_iter().map(|s| s.to_string()).collect(),
            prefix: prefix.to_string(),
            suffix: suffix.to_string(),
            fuzzy_distance: distance,
            distance_budget: budget,
            strict_separators: true,
        }
    }

    #[test]
    fn test_fuzzy_single_exact() {
        let text = "Rust is a programming language";
        let tokens = tokenize_raw(text);
        let params = make_fuzzy_params(vec!["programming"], vec![], "", "", 1, 1);
        assert_eq!(
            count_single_token_fuzzy(text, &tokens, &params, &None, "", test_seg_id(), 0),
            1
        );
    }

    #[test]
    fn test_fuzzy_single_typo() {
        let text = "Rust is a programming language";
        let tokens = tokenize_raw(text);
        let params = make_fuzzy_params(vec!["programing"], vec![], "", "", 1, 1);
        assert_eq!(
            count_single_token_fuzzy(text, &tokens, &params, &None, "", test_seg_id(), 0),
            1
        );
    }

    #[test]
    fn test_fuzzy_single_no_match() {
        let text = "Rust is a programming language";
        let tokens = tokenize_raw(text);
        let params = make_fuzzy_params(vec!["python"], vec![], "", "", 1, 1);
        assert_eq!(
            count_single_token_fuzzy(text, &tokens, &params, &None, "", test_seg_id(), 0),
            0
        );
    }

    #[test]
    fn test_fuzzy_single_multiple_matches() {
        let text = "programming in Rust: a programmer guide";
        let tokens = tokenize_raw(text);
        // "program" is a substring of both "programming" and "programmer"
        let params = make_fuzzy_params(vec!["program"], vec![], "", "", 1, 1);
        assert_eq!(
            count_single_token_fuzzy(text, &tokens, &params, &None, "", test_seg_id(), 0),
            2
        );
    }

    #[test]
    fn test_fuzzy_single_distance_zero_no_match() {
        let text = "Rust is a programming language";
        let tokens = tokenize_raw(text);
        // distance=0: "programing" is not exact, not a substring
        let params = make_fuzzy_params(vec!["programing"], vec![], "", "", 0, 0);
        assert_eq!(
            count_single_token_fuzzy(text, &tokens, &params, &None, "", test_seg_id(), 0),
            0
        );
    }

    #[test]
    fn test_fuzzy_single_highlights() {
        let sink = Arc::new(HighlightSink::new());
        let sid = test_seg_id();
        let text = "Rust programming is fun";
        let tokens = tokenize_raw(text);
        let params = make_fuzzy_params(vec!["programming"], vec![], "", "", 1, 1);
        let tf =
            count_single_token_fuzzy(text, &tokens, &params, &Some(sink.clone()), "", sid, 42);
        assert_eq!(tf, 1);
        let by_field = sink.get(sid, 42).expect("should have highlights");
        let offsets = by_field.get("").expect("should have field offsets");
        assert_eq!(offsets.len(), 1);
        assert_eq!(offsets[0], [5, 16]); // "programming" at bytes 5..16
    }

    // ─── count_multi_token_fuzzy ──────────────────────────────────────

    #[test]
    fn test_fuzzy_multi_exact() {
        let text = "Rust is a systems programming language";
        let tokens = tokenize_raw(text);
        let params =
            make_fuzzy_params(vec!["systems", "programming"], vec![" "], "", "", 1, 1);
        assert_eq!(
            count_multi_token_fuzzy(text, &tokens, &params, &None, "", test_seg_id(), 0),
            1
        );
    }

    #[test]
    fn test_fuzzy_multi_typo() {
        let text = "Rust is a systems programming language";
        let tokens = tokenize_raw(text);
        let params =
            make_fuzzy_params(vec!["sistems", "programing"], vec![" "], "", "", 1, 2);
        assert_eq!(
            count_multi_token_fuzzy(text, &tokens, &params, &None, "", test_seg_id(), 0),
            1
        );
    }

    #[test]
    fn test_fuzzy_multi_budget_exceeded() {
        let text = "Rust is a systems programming language";
        let tokens = tokenize_raw(text);
        // 2 edits total but budget=1
        let params =
            make_fuzzy_params(vec!["sistems", "programing"], vec![" "], "", "", 1, 1);
        assert_eq!(
            count_multi_token_fuzzy(text, &tokens, &params, &None, "", test_seg_id(), 0),
            0
        );
    }

    #[test]
    fn test_fuzzy_multi_no_match() {
        let text = "Rust is a systems programming language";
        let tokens = tokenize_raw(text);
        let params =
            make_fuzzy_params(vec!["machine", "learning"], vec![" "], "", "", 1, 1);
        assert_eq!(
            count_multi_token_fuzzy(text, &tokens, &params, &None, "", test_seg_id(), 0),
            0
        );
    }

    #[test]
    fn test_fuzzy_multi_not_enough_tokens() {
        let text = "Rust";
        let tokens = tokenize_raw(text);
        let params =
            make_fuzzy_params(vec!["systems", "programming"], vec![" "], "", "", 1, 1);
        assert_eq!(
            count_multi_token_fuzzy(text, &tokens, &params, &None, "", test_seg_id(), 0),
            0
        );
    }

    #[test]
    fn test_fuzzy_multi_highlights() {
        let sink = Arc::new(HighlightSink::new());
        let sid = test_seg_id();
        let text = "Rust is a systems programming language";
        let tokens = tokenize_raw(text);
        let params =
            make_fuzzy_params(vec!["systems", "programming"], vec![" "], "", "", 1, 1);
        let tf =
            count_multi_token_fuzzy(text, &tokens, &params, &Some(sink.clone()), "", sid, 42);
        assert_eq!(tf, 1);
        let by_field = sink.get(sid, 42).expect("should have highlights");
        let offsets = by_field.get("").expect("should have field offsets");
        assert_eq!(offsets.len(), 2);
        assert_eq!(offsets[0], [10, 17]); // "systems" at bytes 10..17
        assert_eq!(offsets[1], [18, 29]); // "programming" at bytes 18..29
    }

    // ─── Integration: BooleanQuery + multi-field highlights ──────────────
    //
    // These tests reproduce the segment_ord mismatch bug that caused
    // highlights to be lost for the 2nd sub-query in a BooleanQuery.

    use crate::collector::TopDocs;
    use crate::query::boolean_query::BooleanQuery;
    use crate::query::Occur;
    use crate::schema::{Schema, TextFieldIndexing, TextOptions, STORED};
    use crate::tokenizer::NgramTokenizer;
    use crate::Index;

    /// Build a 2-field index (_title + _content) with raw+ngram variants.
    fn create_two_field_index(docs: &[(&str, &str)]) -> crate::Result<Index> {
        let mut sb = Schema::builder();

        let raw_opts = TextOptions::default()
            .set_indexing_options(
                TextFieldIndexing::default()
                    .set_tokenizer("default")
                    .set_index_option(crate::schema::IndexRecordOption::WithFreqsAndPositionsAndOffsets),
            )
            .set_stored();

        let ngram_opts = TextOptions::default()
            .set_indexing_options(
                TextFieldIndexing::default()
                    .set_tokenizer("ngram3")
                    .set_index_option(crate::schema::IndexRecordOption::WithFreqs),
            );

        let title_raw = sb.add_text_field("_title", raw_opts.clone());
        let title_ngram = sb.add_text_field("_title._ngram", ngram_opts.clone());
        let content_raw = sb.add_text_field("_content", raw_opts);
        let content_ngram = sb.add_text_field("_content._ngram", ngram_opts);

        let schema = sb.build();
        let index = Index::create_in_ram(schema);
        index
            .tokenizers()
            .register("ngram3", NgramTokenizer::all_ngrams(3, 3).unwrap());
        let mut writer = index.writer_for_tests()?;

        for &(title, content) in docs {
            let mut doc = crate::LucivyDocument::new();
            doc.add_text(title_raw, title);
            doc.add_text(title_ngram, title);
            doc.add_text(content_raw, content);
            doc.add_text(content_ngram, content);
            writer.add_document(doc)?;
        }
        writer.commit()?;
        Ok(index)
    }

    /// Regression test: BooleanQuery(should) with NgramContainsQuery on 2 fields.
    /// Before the fix, the 2nd sub-query's highlights were lost because the
    /// HighlightSink used a global counter (next_segment) as key, which gave
    /// different ordinals to sub-queries on the same segment.
    #[test]
    fn test_boolean_multi_field_highlights_not_lost() -> crate::Result<()> {
        let index = create_two_field_index(&[
            ("login page", "authentication and authorization"),
        ])?;
        let schema = index.schema();
        let title_raw = schema.get_field("_title").unwrap();
        let title_ngram = schema.get_field("_title._ngram").unwrap();
        let content_raw = schema.get_field("_content").unwrap();
        let content_ngram = schema.get_field("_content._ngram").unwrap();

        let sink = Arc::new(HighlightSink::new());

        // Build regex-based NgramContainsQuery for "auth" on both fields
        let q_title = NgramContainsQuery::new(
            title_raw,
            title_ngram,
            None,
            vec!["auth".into()],
            VerificationMode::Regex(RegexParams {
                compiled: Regex::new("(?i)auth").unwrap(),
                literals: vec!["auth".into()],
                fuzzy_distance: 0,
            }),
        )
        .with_highlight_sink(sink.clone(), "_title".into());

        let q_content = NgramContainsQuery::new(
            content_raw,
            content_ngram,
            None,
            vec!["auth".into()],
            VerificationMode::Regex(RegexParams {
                compiled: Regex::new("(?i)auth").unwrap(),
                literals: vec!["auth".into()],
                fuzzy_distance: 0,
            }),
        )
        .with_highlight_sink(sink.clone(), "_content".into());

        let bool_query = BooleanQuery::new(vec![
            (Occur::Should, Box::new(q_title)),
            (Occur::Should, Box::new(q_content)),
        ]);

        let reader = index.reader()?;
        let searcher = reader.searcher();
        let top_docs = searcher.search(&bool_query, &TopDocs::with_limit(10).order_by_score())?;

        assert_eq!(top_docs.len(), 1, "should find 1 document");
        let (_score, doc_addr) = top_docs[0];

        // Key assertion: use the real SegmentId (same as what the scorer uses)
        let seg_id = searcher.segment_reader(doc_addr.segment_ord).segment_id();
        let by_field = sink.get(seg_id, doc_addr.doc_id)
            .expect("highlights should exist for matching document");

        // "auth" appears in _content ("authentication") but NOT in _title ("login page")
        assert!(
            by_field.contains_key("_content"),
            "should have _content highlights, got: {:?}",
            by_field.keys().collect::<Vec<_>>()
        );
        let content_offsets = &by_field["_content"];
        assert!(!content_offsets.is_empty(), "content highlights should not be empty");
        // "authentication" starts at byte 0 in "authentication and authorization"
        assert_eq!(content_offsets[0][0], 0);

        Ok(())
    }

    /// Test that both fields get highlights when the query matches in both.
    #[test]
    fn test_boolean_both_fields_highlighted() -> crate::Result<()> {
        let index = create_two_field_index(&[
            ("source code review", "the source of truth"),
        ])?;
        let schema = index.schema();
        let title_raw = schema.get_field("_title").unwrap();
        let title_ngram = schema.get_field("_title._ngram").unwrap();
        let content_raw = schema.get_field("_content").unwrap();
        let content_ngram = schema.get_field("_content._ngram").unwrap();

        let sink = Arc::new(HighlightSink::new());

        let q_title = NgramContainsQuery::new(
            title_raw, title_ngram, None,
            vec!["source".into()],
            VerificationMode::Regex(RegexParams {
                compiled: Regex::new("(?i)source").unwrap(),
                literals: vec!["source".into()],
                fuzzy_distance: 0,
            }),
        ).with_highlight_sink(sink.clone(), "_title".into());

        let q_content = NgramContainsQuery::new(
            content_raw, content_ngram, None,
            vec!["source".into()],
            VerificationMode::Regex(RegexParams {
                compiled: Regex::new("(?i)source").unwrap(),
                literals: vec!["source".into()],
                fuzzy_distance: 0,
            }),
        ).with_highlight_sink(sink.clone(), "_content".into());

        let bool_query = BooleanQuery::new(vec![
            (Occur::Should, Box::new(q_title)),
            (Occur::Should, Box::new(q_content)),
        ]);

        let reader = index.reader()?;
        let searcher = reader.searcher();
        let top_docs = searcher.search(&bool_query, &TopDocs::with_limit(10).order_by_score())?;

        assert_eq!(top_docs.len(), 1);
        let (_score, doc_addr) = top_docs[0];

        let seg_id = searcher.segment_reader(doc_addr.segment_ord).segment_id();
        let by_field = sink.get(seg_id, doc_addr.doc_id)
            .expect("highlights should exist");

        assert!(
            by_field.contains_key("_title"),
            "should have _title highlights, got: {:?}", by_field.keys().collect::<Vec<_>>()
        );
        assert!(
            by_field.contains_key("_content"),
            "should have _content highlights, got: {:?}", by_field.keys().collect::<Vec<_>>()
        );

        // "source" in "_title" = "source code review" → offset [0, 6]
        assert_eq!(by_field["_title"][0], [0, 6]);
        // "source" in "_content" = "the source of truth" → offset [4, 10]
        assert_eq!(by_field["_content"][0], [4, 10]);

        Ok(())
    }
}
