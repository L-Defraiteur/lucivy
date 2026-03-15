//! RegexContinuationQuery — regex/fuzzy search across token boundaries via
//! chained DFA walks through the suffix FST and GapMap.
//!
//! Instead of matching a regex against individual tokens, this query walks
//! the DFA through token → gap → token → gap chains, finding matches that
//! span multiple tokens without ever touching stored text.

use std::collections::HashMap;
use std::sync::Arc;

use common::BitSet;
use levenshtein_automata::LevenshteinAutomatonBuilder;
use lucivy_fst::Automaton;
use once_cell::sync::OnceCell;
use tantivy_fst::Regex;

use crate::docset::DocSet;
use crate::index::SegmentReader;
use crate::postings::Postings;
use crate::query::automaton_weight::SfxAutomatonAdapter;
use crate::query::phrase_query::scoring_utils::HighlightSink;
use crate::query::{BitSetDocSet, ConstScorer, EnableScoring, Explanation, Query, Scorer, Weight};
use crate::schema::{Field, IndexRecordOption};
use crate::suffix_fst::file::{SfxDfaWrapper, SfxFileReader};
use crate::suffix_fst::gapmap::is_value_boundary;
use crate::suffix_fst::SfxTermDictionary;
use crate::{DocId, LucivyError, Score, TERMINATED};

/// Mode controls where the regex can match relative to the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContinuationMode {
    /// Regex can match anywhere (any SI for initial walk).
    Contains,
    /// Regex must match from the start of the first token (SI=0 only).
    StartsWith,
}

/// Maximum continuation depth (token boundaries to traverse).
const MAX_CONTINUATION_DEPTH: usize = 64;

/// Cached LevenshteinAutomatonBuilder.
fn get_builder(distance: u8) -> &'static LevenshteinAutomatonBuilder {
    static BUILDERS: [OnceCell<LevenshteinAutomatonBuilder>; 4] = [
        OnceCell::new(),
        OnceCell::new(),
        OnceCell::new(),
        OnceCell::new(),
    ];
    BUILDERS[distance as usize].get_or_init(|| LevenshteinAutomatonBuilder::new(distance, true))
}

/// What kind of DFA to build for the continuation walk.
#[derive(Debug, Clone)]
enum DfaKind {
    /// Levenshtein DFA: exact or fuzzy match on a literal string.
    Fuzzy { text: String, distance: u8 },
    /// Regex DFA: compile a regex pattern into an automaton.
    Regex { pattern: String },
}

/// A query that matches a DFA (Levenshtein or regex) across token boundaries
/// by chaining suffix FST walks through GapMap separators.
#[derive(Debug, Clone)]
pub struct RegexContinuationQuery {
    field: Field,
    dfa_kind: DfaKind,
    mode: ContinuationMode,
    highlight_sink: Option<Arc<HighlightSink>>,
    highlight_field_name: String,
}

impl RegexContinuationQuery {
    /// Continuation with Levenshtein DFA (exact or fuzzy).
    pub fn new(field: Field, query_text: String, mode: ContinuationMode) -> Self {
        Self {
            field,
            dfa_kind: DfaKind::Fuzzy { text: query_text, distance: 0 },
            mode,
            highlight_sink: None,
            highlight_field_name: String::new(),
        }
    }

    /// Set fuzzy distance for Levenshtein mode.
    pub fn with_fuzzy_distance(mut self, dist: u8) -> Self {
        if let DfaKind::Fuzzy { ref mut distance, .. } = self.dfa_kind {
            *distance = dist;
        }
        self
    }

    /// Continuation with a regex pattern DFA.
    pub fn from_regex(field: Field, pattern: String, mode: ContinuationMode) -> Self {
        Self {
            field,
            dfa_kind: DfaKind::Regex { pattern },
            mode,
            highlight_sink: None,
            highlight_field_name: String::new(),
        }
    }

    /// Attach a highlight sink for collecting byte offsets of matches.
    pub fn with_highlight_sink(mut self, sink: Arc<HighlightSink>, field_name: String) -> Self {
        self.highlight_sink = Some(sink);
        self.highlight_field_name = field_name;
        self
    }
}

impl Query for RegexContinuationQuery {
    fn weight(&self, _enable_scoring: EnableScoring<'_>) -> crate::Result<Box<dyn Weight>> {
        Ok(Box::new(RegexContinuationWeight {
            field: self.field,
            dfa_kind: self.dfa_kind.clone(),
            mode: self.mode,
            highlight_sink: self.highlight_sink.clone(),
            highlight_field_name: self.highlight_field_name.clone(),
        }))
    }
}

struct RegexContinuationWeight {
    field: Field,
    dfa_kind: DfaKind,
    mode: ContinuationMode,
    highlight_sink: Option<Arc<HighlightSink>>,
    highlight_field_name: String,
}

/// Candidate state: DFA end state + byte_from of match start for highlights.
#[derive(Clone)]
struct CandidateState<S> {
    dfa_state: S,
    byte_from: u32,
}

/// Run the continuation algorithm with a given automaton on a segment.
/// Returns (doc_bitset, highlights) where highlights = Vec<(doc_id, byte_from, byte_to)>.
fn continuation_score<A: Automaton>(
    automaton: &A,
    sfx_dict: &SfxTermDictionary,
    inverted_index: &crate::index::InvertedIndexReader,
    sfx_reader: &SfxFileReader,
    mode: ContinuationMode,
    max_doc: DocId,
) -> crate::Result<(BitSet, Vec<(DocId, usize, usize)>)>
where
    A::State: Clone + Eq + std::hash::Hash,
{
    let mut doc_bitset = BitSet::with_max_value(max_doc);
    let mut highlights: Vec<(DocId, usize, usize)> = Vec::new();
    let gapmap = sfx_reader.gapmap();

    // === Walk 1: initial walk ===
    let si_zero_only = mode != ContinuationMode::Contains;
    let start_state = automaton.start();
    let matches = sfx_dict.search_continuation(automaton, start_state, si_zero_only);

    // Candidates: (doc_id, position) → list of (DFA state, byte_from).
    let mut candidates: HashMap<(DocId, u32), Vec<CandidateState<A::State>>> = HashMap::new();

    for m in &matches {
        let mut postings = inverted_index.read_postings_from_terminfo(
            &m.term_info,
            IndexRecordOption::WithFreqsAndPositionsAndOffsets,
        )?;

        loop {
            let doc = postings.doc();
            if doc == TERMINATED {
                break;
            }

            let mut pos_offsets = Vec::new();
            postings.append_positions_and_offsets(0, &mut pos_offsets);

            for &(pos, off_from, off_to) in &pos_offsets {
                // byte_from = token start + SI (suffix offset within token)
                let byte_from = off_from + m.si as u32;

                if m.is_accepting {
                    doc_bitset.insert(doc);
                    highlights.push((doc, byte_from as usize, off_to as usize));
                } else if automaton.can_match(&m.end_state) {
                    let states = candidates.entry((doc, pos)).or_default();
                    let cs = CandidateState { dfa_state: m.end_state.clone(), byte_from };
                    if !states.iter().any(|s| s.dfa_state == m.end_state) {
                        states.push(cs);
                    }
                }
            }

            postings.advance();
        }
    }

    // === Continuation loop ===
    for _depth in 0..MAX_CONTINUATION_DEPTH {
        if candidates.is_empty() {
            break;
        }

        // Feed gap bytes to DFA for each candidate → group by post-gap state
        // post_gap: state → Vec<(doc, next_pos, byte_from)>
        let mut post_gap: HashMap<A::State, Vec<(DocId, u32, u32)>> = HashMap::new();

        for (&(doc, pos), cand_states) in &candidates {
            let gap = gapmap.read_separator(doc, pos, pos + 1);
            let Some(gap_bytes) = gap else {
                continue;
            };
            if is_value_boundary(gap_bytes) {
                continue;
            }

            for cs in cand_states {
                let mut state = cs.dfa_state.clone();
                let mut alive = true;
                for &byte in gap_bytes {
                    state = automaton.accept(&state, byte);
                    if !automaton.can_match(&state) {
                        alive = false;
                        break;
                    }
                }

                if !alive {
                    continue;
                }

                if automaton.is_match(&state) {
                    doc_bitset.insert(doc);
                    // Match ends in the gap — byte_to is end of previous token + gap length.
                    // Approximate: use the offset of the next token's start (if available).
                    // For now, we don't record a highlight for gap-only endings.
                }
                if automaton.can_match(&state) {
                    post_gap.entry(state).or_default().push((doc, pos + 1, cs.byte_from));
                }
            }
        }

        if post_gap.is_empty() {
            break;
        }

        // Walk FST for each unique post-gap state (SI=0, continuation tokens)
        let mut new_candidates: HashMap<(DocId, u32), Vec<CandidateState<A::State>>> = HashMap::new();

        for (gap_state, doc_positions) in &post_gap {
            let next_matches =
                sfx_dict.search_continuation(automaton, gap_state.clone(), true);

            // Build candidate doc → expected (pos, byte_from) for fast intersection
            let candidate_docs: HashMap<DocId, Vec<(u32, u32)>> = {
                let mut map: HashMap<DocId, Vec<(u32, u32)>> = HashMap::new();
                for &(doc, expected_pos, byte_from) in doc_positions {
                    map.entry(doc).or_default().push((expected_pos, byte_from));
                }
                map
            };

            for nm in &next_matches {
                let mut postings = inverted_index.read_postings_from_terminfo(
                    &nm.term_info,
                    IndexRecordOption::WithFreqsAndPositionsAndOffsets,
                )?;

                loop {
                    let doc = postings.doc();
                    if doc == TERMINATED {
                        break;
                    }

                    if let Some(expected) = candidate_docs.get(&doc) {
                        let mut pos_offsets = Vec::new();
                        postings.append_positions_and_offsets(0, &mut pos_offsets);

                        for &(pos, _off_from, off_to) in &pos_offsets {
                            for &(exp_pos, byte_from) in expected {
                                if pos == exp_pos {
                                    if nm.is_accepting {
                                        doc_bitset.insert(doc);
                                        highlights.push((doc, byte_from as usize, off_to as usize));
                                    } else if automaton.can_match(&nm.end_state) {
                                        let states = new_candidates
                                            .entry((doc, pos))
                                            .or_default();
                                        let cs = CandidateState {
                                            dfa_state: nm.end_state.clone(),
                                            byte_from,
                                        };
                                        if !states.iter().any(|s| s.dfa_state == nm.end_state) {
                                            states.push(cs);
                                        }
                                    }
                                }
                            }
                        }
                    }

                    postings.advance();
                }
            }
        }

        candidates = new_candidates;
    }

    Ok((doc_bitset, highlights))
}

impl Weight for RegexContinuationWeight {
    fn scorer(&self, reader: &SegmentReader, boost: Score) -> crate::Result<Box<dyn Scorer>> {
        let max_doc = reader.max_doc();

        // Open .sfx
        let sfx_data = reader.sfx_file(self.field).ok_or_else(|| {
            LucivyError::InvalidArgument(format!(
                "no .sfx file for field {:?}. RegexContinuationQuery requires suffix index.",
                self.field
            ))
        })?;
        let sfx_bytes = sfx_data
            .read_bytes()
            .map_err(|e| LucivyError::SystemError(format!("read .sfx: {e}")))?;
        let sfx_reader = SfxFileReader::open(sfx_bytes.as_ref())
            .map_err(|e| LucivyError::SystemError(format!("open .sfx: {e}")))?;

        let inverted_index = reader.inverted_index(self.field)?;
        let sfx_dict = SfxTermDictionary::new(&sfx_reader, inverted_index.terms());

        let (doc_bitset, highlights) = match &self.dfa_kind {
            DfaKind::Fuzzy { text, distance } => {
                let builder = get_builder(*distance);
                let dfa = builder.build_dfa(text);
                let automaton = SfxDfaWrapper(dfa);
                continuation_score(
                    &automaton, &sfx_dict, &inverted_index, &sfx_reader, self.mode, max_doc,
                )?
            }
            DfaKind::Regex { pattern } => {
                let regex = Regex::new(pattern).map_err(|e| {
                    LucivyError::InvalidArgument(format!("RegexContinuation: {e}"))
                })?;
                let automaton = SfxAutomatonAdapter(&regex);
                continuation_score(
                    &automaton, &sfx_dict, &inverted_index, &sfx_reader, self.mode, max_doc,
                )?
            }
        };

        // Report highlights to sink
        if let Some(ref sink) = self.highlight_sink {
            let segment_id = reader.segment_id();
            for &(doc_id, byte_from, byte_to) in &highlights {
                sink.insert(
                    segment_id,
                    doc_id,
                    &self.highlight_field_name,
                    vec![[byte_from, byte_to]],
                );
            }
        }

        let doc_bitset = BitSetDocSet::from(doc_bitset);
        let scorer = ConstScorer::new(doc_bitset, boost);
        Ok(Box::new(scorer))
    }

    fn explain(&self, reader: &SegmentReader, doc: DocId) -> crate::Result<Explanation> {
        let mut scorer = self.scorer(reader, 1.0)?;
        if scorer.seek(doc) == doc {
            Ok(Explanation::new("RegexContinuationQuery", 1.0))
        } else {
            Err(LucivyError::InvalidArgument(
                "Document does not exist".to_string(),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collector::TopDocs;
    use crate::schema::{SchemaBuilder, TextFieldIndexing, TextOptions};
    use crate::tokenizer::{LowerCaser, SimpleTokenizer, TextAnalyzer};
    use crate::{Index, LucivyDocument};

    fn build_continuation_index() -> (Index, Field) {
        let mut schema_builder = SchemaBuilder::new();
        let raw_opts = TextOptions::default().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer("raw")
                .set_index_option(IndexRecordOption::WithFreqsAndPositionsAndOffsets),
        );
        let field = schema_builder.add_text_field("body._raw", raw_opts);
        let schema = schema_builder.build();

        let index = Index::create_in_ram(schema);
        let raw_tokenizer = TextAnalyzer::builder(SimpleTokenizer::default())
            .filter(LowerCaser)
            .build();
        index.tokenizers().register("raw", raw_tokenizer);

        let mut writer = index.writer_for_tests().unwrap();

        // Doc 0: "import rag3db from core"
        let mut doc = LucivyDocument::new();
        doc.add_text(field, "import rag3db from core");
        writer.add_document(doc).unwrap();

        // Doc 1: "rag3db is cool"
        let mut doc = LucivyDocument::new();
        doc.add_text(field, "rag3db is cool");
        writer.add_document(doc).unwrap();

        // Doc 2: "nothing here"
        let mut doc = LucivyDocument::new();
        doc.add_text(field, "nothing here");
        writer.add_document(doc).unwrap();

        writer.commit().unwrap();

        (index, field)
    }

    #[test]
    fn test_continuation_single_token_match() {
        let (index, field) = build_continuation_index();
        let query = RegexContinuationQuery::new(
            field,
            "rag3db".into(),
            ContinuationMode::Contains,
        );
        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        let results = searcher
            .search(&query, &TopDocs::with_limit(10).order_by_score())
            .unwrap();

        assert_eq!(results.len(), 2, "rag3db should match 2 docs");
    }

    #[test]
    fn test_continuation_cross_token_exact() {
        // "rag3db is cool" spans 3 tokens with spaces as gaps
        let (index, field) = build_continuation_index();
        let query = RegexContinuationQuery::new(
            field,
            "rag3db is cool".into(),
            ContinuationMode::StartsWith,
        );
        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        let results = searcher
            .search(&query, &TopDocs::with_limit(10).order_by_score())
            .unwrap();

        assert_eq!(results.len(), 1, "should match doc 1 only");
        assert_eq!(results[0].1.doc_id, 1);
    }

    #[test]
    fn test_continuation_cross_token_fuzzy() {
        // Fuzzy "rag3db iz cool" d=1 → should match "rag3db is cool"
        let (index, field) = build_continuation_index();
        let query = RegexContinuationQuery::new(
            field,
            "rag3db iz cool".into(),
            ContinuationMode::StartsWith,
        )
        .with_fuzzy_distance(1);
        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        let results = searcher
            .search(&query, &TopDocs::with_limit(10).order_by_score())
            .unwrap();

        assert_eq!(results.len(), 1, "fuzzy d=1 should match doc 1");
        assert_eq!(results[0].1.doc_id, 1);
    }

    #[test]
    fn test_continuation_no_match() {
        let (index, field) = build_continuation_index();
        let query = RegexContinuationQuery::new(
            field,
            "rag3db is warm".into(),
            ContinuationMode::StartsWith,
        );
        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        let results = searcher
            .search(&query, &TopDocs::with_limit(10).order_by_score())
            .unwrap();

        assert_eq!(results.len(), 0);
    }

    #[test]
    fn test_continuation_contains_mid_token() {
        // "3db is" starts mid-token "rag3db" at SI=3, crosses gap to "is"
        let (index, field) = build_continuation_index();
        let query = RegexContinuationQuery::new(
            field,
            "3db is".into(),
            ContinuationMode::Contains,
        );
        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        let results = searcher
            .search(&query, &TopDocs::with_limit(10).order_by_score())
            .unwrap();

        assert_eq!(results.len(), 1, "contains '3db is' should match doc 1");
        assert_eq!(results[0].1.doc_id, 1);
    }

    #[test]
    fn test_continuation_regex_cross_token() {
        // Regex "rag.db i. cool" — . matches any char, spans 3 tokens
        let (index, field) = build_continuation_index();
        let query = RegexContinuationQuery::from_regex(
            field,
            "rag.db i. cool".into(),
            ContinuationMode::StartsWith,
        );
        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        let results = searcher
            .search(&query, &TopDocs::with_limit(10).order_by_score())
            .unwrap();

        assert_eq!(results.len(), 1, "regex should match doc 1");
        assert_eq!(results[0].1.doc_id, 1);
    }

    #[test]
    fn test_continuation_regex_contains_mid_token() {
        // Regex "3db i." — starts mid-token, crosses gap, . matches 's'
        let (index, field) = build_continuation_index();
        let query = RegexContinuationQuery::from_regex(
            field,
            "3db i.".into(),
            ContinuationMode::Contains,
        );
        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        let results = searcher
            .search(&query, &TopDocs::with_limit(10).order_by_score())
            .unwrap();

        assert_eq!(results.len(), 1, "regex contains '3db i.' should match doc 1");
        assert_eq!(results[0].1.doc_id, 1);
    }

    #[test]
    fn test_continuation_regex_no_match() {
        // Regex "rag.db x. cool" — 'x.' won't match 'is'
        let (index, field) = build_continuation_index();
        let query = RegexContinuationQuery::from_regex(
            field,
            "rag.db x. cool".into(),
            ContinuationMode::StartsWith,
        );
        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        let results = searcher
            .search(&query, &TopDocs::with_limit(10).order_by_score())
            .unwrap();

        assert_eq!(results.len(), 0);
    }

    // ── Fuzzy single-token query → continuation tests ──
    // These verify that a single-token fuzzy query correctly spans token
    // boundaries when the edit distance budget absorbs the gap.

    #[test]
    fn test_fuzzy_single_token_absorbs_gap() {
        // "importrag3db" d=1 → should match "import rag3db" (space = 1 insertion)
        let (index, field) = build_continuation_index();
        let query = RegexContinuationQuery::new(
            field,
            "importrag3db".into(),
            ContinuationMode::StartsWith,
        )
        .with_fuzzy_distance(1);
        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        let results = searcher
            .search(&query, &TopDocs::with_limit(10).order_by_score())
            .unwrap();

        // Doc 0 has "import rag3db from core" — "import rag3db" matches with d=1
        assert_eq!(results.len(), 1, "'importrag3db' d=1 should match 'import rag3db'");
        assert_eq!(results[0].1.doc_id, 0);
    }

    #[test]
    fn test_fuzzy_single_token_no_gap_budget() {
        // "importrag3db" d=0 → should NOT match "import rag3db" (no budget for gap)
        let (index, field) = build_continuation_index();
        let query = RegexContinuationQuery::new(
            field,
            "importrag3db".into(),
            ContinuationMode::StartsWith,
        );
        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        let results = searcher
            .search(&query, &TopDocs::with_limit(10).order_by_score())
            .unwrap();

        assert_eq!(results.len(), 0, "d=0 cannot absorb the gap");
    }

    #[test]
    fn test_fuzzy_single_token_still_matches_single() {
        // "rag3dc" d=1 → should still match "rag3db" (single token, no continuation)
        let (index, field) = build_continuation_index();
        let query = RegexContinuationQuery::new(
            field,
            "rag3dc".into(),
            ContinuationMode::Contains,
        )
        .with_fuzzy_distance(1);
        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        let results = searcher
            .search(&query, &TopDocs::with_limit(10).order_by_score())
            .unwrap();

        assert_eq!(results.len(), 2, "fuzzy 'rag3dc' d=1 should match 2 docs with 'rag3db'");
    }

    #[test]
    fn test_fuzzy_contains_absorbs_gap_mid_token() {
        // "3dbis" d=1 contains → "3db" (suffix SI=3) + gap " " (insertion d=1) + "is"
        let (index, field) = build_continuation_index();
        let query = RegexContinuationQuery::new(
            field,
            "3dbis".into(),
            ContinuationMode::Contains,
        )
        .with_fuzzy_distance(1);
        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        let results = searcher
            .search(&query, &TopDocs::with_limit(10).order_by_score())
            .unwrap();

        // Doc 1: "rag3db is cool" → suffix "3db" + " " gap + "is"
        assert_eq!(results.len(), 1, "'3dbis' d=1 contains should match doc 1");
        assert_eq!(results[0].1.doc_id, 1);
    }

    #[test]
    fn test_fuzzy_three_tokens_d2() {
        // "rag3dbiscool" d=2 → "rag3db" + " "(d=1) + "is" + " "(d=2) + "cool"
        let (index, field) = build_continuation_index();
        let query = RegexContinuationQuery::new(
            field,
            "rag3dbiscool".into(),
            ContinuationMode::StartsWith,
        )
        .with_fuzzy_distance(2);
        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        let results = searcher
            .search(&query, &TopDocs::with_limit(10).order_by_score())
            .unwrap();

        // Doc 1: "rag3db is cool" — 2 spaces absorbed = d=2
        assert_eq!(results.len(), 1, "'rag3dbiscool' d=2 should match 'rag3db is cool'");
        assert_eq!(results[0].1.doc_id, 1);
    }

    // ── Highlight tests ──

    #[test]
    fn test_highlights_single_token() {
        // "rag3db" exact → highlight [0,6] in doc 1 ("rag3db is cool")
        let (index, field) = build_continuation_index();
        let sink = Arc::new(HighlightSink::default());
        let query = RegexContinuationQuery::new(
            field,
            "rag3db".into(),
            ContinuationMode::StartsWith,
        )
        .with_highlight_sink(Arc::clone(&sink), "body._raw".into());

        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        let _ = searcher
            .search(&query, &TopDocs::with_limit(10).order_by_score())
            .unwrap();

        let all = sink.all_entries();
        // Doc 1: "rag3db" at offset [0, 6]
        let doc1_entries: Vec<_> = all.iter().filter(|e| e.doc_id == 1).collect();
        assert!(!doc1_entries.is_empty(), "should have highlights for doc 1");
        assert!(
            doc1_entries.iter().any(|e| e.offsets.contains(&[0, 6])),
            "doc 1 should have highlight [0,6] for 'rag3db', got {:?}", doc1_entries
        );
    }

    #[test]
    fn test_highlights_cross_token() {
        // "rag3db is cool" exact → highlight [0,14] in doc 1
        let (index, field) = build_continuation_index();
        let sink = Arc::new(HighlightSink::default());
        let query = RegexContinuationQuery::new(
            field,
            "rag3db is cool".into(),
            ContinuationMode::StartsWith,
        )
        .with_highlight_sink(Arc::clone(&sink), "body._raw".into());

        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        let results = searcher
            .search(&query, &TopDocs::with_limit(10).order_by_score())
            .unwrap();
        assert_eq!(results.len(), 1);

        let all = sink.all_entries();
        let doc1_entries: Vec<_> = all.iter().filter(|e| e.doc_id == 1).collect();
        assert!(!doc1_entries.is_empty(), "should have highlights for doc 1");
        // "rag3db is cool" = bytes [0, 14]
        assert!(
            doc1_entries.iter().any(|e| e.offsets.contains(&[0, 14])),
            "doc 1 should have highlight [0,14], got {:?}", doc1_entries
        );
    }

    #[test]
    fn test_highlights_contains_mid_token() {
        // "3db is" contains → highlight starts at byte 3 (SI=3 of "rag3db")
        // "rag3db is cool" → "3db" starts at byte 3, "is" ends at byte 10
        let (index, field) = build_continuation_index();
        let sink = Arc::new(HighlightSink::default());
        let query = RegexContinuationQuery::new(
            field,
            "3db is".into(),
            ContinuationMode::Contains,
        )
        .with_highlight_sink(Arc::clone(&sink), "body._raw".into());

        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        let results = searcher
            .search(&query, &TopDocs::with_limit(10).order_by_score())
            .unwrap();
        assert_eq!(results.len(), 1);

        let all = sink.all_entries();
        let doc1_entries: Vec<_> = all.iter().filter(|e| e.doc_id == 1).collect();
        assert!(!doc1_entries.is_empty(), "should have highlights for doc 1");
        // "3db is" in "rag3db is cool": byte 3 to byte 9
        // r(0)a(1)g(2)3(3)d(4)b(5) (6)i(7)s(8) → end offset 9
        assert!(
            doc1_entries.iter().any(|e| {
                e.offsets.iter().any(|o| o[0] == 3 && o[1] == 9)
            }),
            "doc 1 should have highlight [3,9], got {:?}", doc1_entries
        );
    }
}
