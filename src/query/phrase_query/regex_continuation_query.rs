//! RegexContinuationQuery — regex/fuzzy search across token boundaries via
//! chained DFA walks through the suffix FST and GapMap.
//!
//! Instead of matching a regex against individual tokens, this query walks
//! the DFA through token → gap → token → gap chains, finding matches that
//! span multiple tokens without ever touching stored text.

use std::collections::HashMap;

use common::BitSet;
use levenshtein_automata::LevenshteinAutomatonBuilder;
use lucivy_fst::Automaton;
use once_cell::sync::OnceCell;

use crate::docset::DocSet;
use crate::index::SegmentReader;
use crate::postings::Postings;
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

/// A query that matches a Levenshtein DFA across token boundaries by chaining
/// suffix FST walks through GapMap separators.
#[derive(Debug, Clone)]
pub struct RegexContinuationQuery {
    field: Field,
    query_text: String,
    fuzzy_distance: u8,
    mode: ContinuationMode,
}

impl RegexContinuationQuery {
    pub fn new(field: Field, query_text: String, mode: ContinuationMode) -> Self {
        Self {
            field,
            query_text,
            fuzzy_distance: 0,
            mode,
        }
    }

    pub fn with_fuzzy_distance(mut self, distance: u8) -> Self {
        self.fuzzy_distance = distance;
        self
    }
}

impl Query for RegexContinuationQuery {
    fn weight(&self, _enable_scoring: EnableScoring<'_>) -> crate::Result<Box<dyn Weight>> {
        Ok(Box::new(RegexContinuationWeight {
            field: self.field,
            query_text: self.query_text.clone(),
            fuzzy_distance: self.fuzzy_distance,
            mode: self.mode,
        }))
    }
}

struct RegexContinuationWeight {
    field: Field,
    query_text: String,
    fuzzy_distance: u8,
    mode: ContinuationMode,
}

/// Run the continuation algorithm with a given automaton on a segment.
fn continuation_score<A: Automaton>(
    automaton: &A,
    sfx_dict: &SfxTermDictionary,
    inverted_index: &crate::index::InvertedIndexReader,
    sfx_reader: &SfxFileReader,
    mode: ContinuationMode,
    max_doc: DocId,
) -> crate::Result<BitSet>
where
    A::State: Clone + Eq + std::hash::Hash,
{
    let mut doc_bitset = BitSet::with_max_value(max_doc);
    let gapmap = sfx_reader.gapmap();

    // === Walk 1: initial walk ===
    let si_zero_only = mode != ContinuationMode::Contains;
    let start_state = automaton.start();
    let matches = sfx_dict.search_continuation(automaton, start_state, si_zero_only);

    // Candidates: (doc_id, position) → DFA end state
    let mut candidates: HashMap<(DocId, u32), A::State> = HashMap::new();

    for m in &matches {
        let mut postings = inverted_index.read_postings_from_terminfo(
            &m.term_info,
            IndexRecordOption::WithFreqsAndPositions,
        )?;

        loop {
            let doc = postings.doc();
            if doc == TERMINATED {
                break;
            }

            let mut positions = Vec::new();
            postings.append_positions_with_offset(0, &mut positions);

            for &pos in &positions {
                if m.is_accepting {
                    doc_bitset.insert(doc);
                } else if automaton.can_match(&m.end_state) {
                    candidates.insert((doc, pos as u32), m.end_state.clone());
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
        let mut post_gap: HashMap<A::State, Vec<(DocId, u32)>> = HashMap::new();

        for (&(doc, pos), end_state) in &candidates {
            let gap = gapmap.read_separator(doc, pos, pos + 1);
            let Some(gap_bytes) = gap else {
                continue;
            };
            if is_value_boundary(gap_bytes) {
                continue;
            }

            // Feed gap bytes to DFA
            let mut state = end_state.clone();
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
            }
            if automaton.can_match(&state) {
                post_gap.entry(state).or_default().push((doc, pos + 1));
            }
        }

        if post_gap.is_empty() {
            break;
        }

        // Walk FST for each unique post-gap state (SI=0, continuation tokens)
        let mut new_candidates: HashMap<(DocId, u32), A::State> = HashMap::new();

        for (gap_state, doc_positions) in &post_gap {
            let next_matches =
                sfx_dict.search_continuation(automaton, gap_state.clone(), true);

            // Build candidate doc → expected positions for fast intersection
            let candidate_docs: HashMap<DocId, Vec<u32>> = {
                let mut map: HashMap<DocId, Vec<u32>> = HashMap::new();
                for &(doc, expected_pos) in doc_positions {
                    map.entry(doc).or_default().push(expected_pos);
                }
                map
            };

            for nm in &next_matches {
                let mut postings = inverted_index.read_postings_from_terminfo(
                    &nm.term_info,
                    IndexRecordOption::WithFreqsAndPositions,
                )?;

                loop {
                    let doc = postings.doc();
                    if doc == TERMINATED {
                        break;
                    }

                    if let Some(expected_positions) = candidate_docs.get(&doc) {
                        let mut positions = Vec::new();
                        postings.append_positions_with_offset(0, &mut positions);

                        for &pos in &positions {
                            if expected_positions.contains(&(pos as u32)) {
                                if nm.is_accepting {
                                    doc_bitset.insert(doc);
                                } else if automaton.can_match(&nm.end_state) {
                                    new_candidates
                                        .insert((doc, pos as u32), nm.end_state.clone());
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

    Ok(doc_bitset)
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

        // Build DFA
        let builder = get_builder(self.fuzzy_distance);
        let dfa = builder.build_dfa(&self.query_text);
        let automaton = SfxDfaWrapper(dfa);

        let doc_bitset = continuation_score(
            &automaton,
            &sfx_dict,
            &inverted_index,
            &sfx_reader,
            self.mode,
            max_doc,
        )?;

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
}
