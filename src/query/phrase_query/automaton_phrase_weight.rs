use std::sync::Arc;

use common::BitSet;
use levenshtein_automata::LevenshteinAutomatonBuilder;
use once_cell::sync::OnceCell;
use tantivy_fst::Regex;

use super::contains_scorer::{ContainsScorer, ContainsSingleScorer};
use super::regex_phrase_weight::RegexPhraseWeight;
use super::scoring_utils::HighlightSink;
use super::PhraseScorer;
use crate::fieldnorm::FieldNormReader;
use crate::index::{SegmentId, SegmentReader};
use crate::postings::TermInfo;
use crate::query::bm25::Bm25Weight;
use crate::query::explanation::does_not_match;
use crate::query::fuzzy_query::DfaWrapper;
use crate::query::fuzzy_substring_automaton::FuzzySubstringAutomaton;
use crate::query::{BitSetDocSet, ConstScorer, EmptyScorer, Explanation, Scorer, Weight};
use crate::schema::{Field, IndexRecordOption, Term};
use crate::{DocId, InvertedIndexReader, Score};

/// Cascade level returned by cascade_term_infos.
#[derive(Debug, Clone, Copy)]
pub(crate) enum CascadeLevel {
    Exact,
    Fuzzy(u8),
    Substring,
    FuzzySubstring(u8),
}

impl CascadeLevel {
    pub fn distance(&self) -> u32 {
        match self {
            CascadeLevel::Exact => 0,
            CascadeLevel::Fuzzy(d) => *d as u32,
            CascadeLevel::Substring => 0,
            CascadeLevel::FuzzySubstring(d) => *d as u32,
        }
    }
}

/// Weight for `AutomatonPhraseQuery`. Implements the auto-cascade
/// (exact → fuzzy → substring → fuzzy substring) per position, then delegates to
/// `ContainsScorer` (with separator validation) or `PhraseScorer` for multi-token,
/// or a `ConstScorer`/`ContainsSingleScorer` for single-token.
pub struct AutomatonPhraseWeight {
    field: Field,
    /// Field to load stored text from for separator validation.
    stored_field: Option<Field>,
    phrase_terms: Vec<(usize, String)>,
    similarity_weight_opt: Option<Bm25Weight>,
    max_expansions: u32,
    fuzzy_distance: u8,
    query_separators: Vec<String>,
    query_prefix: String,
    query_suffix: String,
    distance_budget: u32,
    strict_separators: bool,
    highlight_sink: Option<Arc<HighlightSink>>,
    highlight_field_name: String,
}

impl AutomatonPhraseWeight {
    pub fn new(
        field: Field,
        stored_field: Option<Field>,
        phrase_terms: Vec<(usize, String)>,
        similarity_weight_opt: Option<Bm25Weight>,
        max_expansions: u32,
        fuzzy_distance: u8,
        query_separators: Vec<String>,
        query_prefix: String,
        query_suffix: String,
        distance_budget: u32,
        strict_separators: bool,
        highlight_sink: Option<Arc<HighlightSink>>,
        highlight_field_name: String,
    ) -> Self {
        AutomatonPhraseWeight {
            field,
            stored_field,
            phrase_terms,
            similarity_weight_opt,
            max_expansions,
            fuzzy_distance,
            query_separators,
            query_prefix,
            query_suffix,
            distance_budget,
            strict_separators,
            highlight_sink,
            highlight_field_name,
        }
    }

    /// Returns true if separator/prefix/suffix validation is needed.
    fn needs_validation(&self) -> bool {
        !self.query_separators.is_empty()
            || !self.query_prefix.is_empty()
            || !self.query_suffix.is_empty()
    }

    fn fieldnorm_reader(&self, reader: &SegmentReader) -> crate::Result<FieldNormReader> {
        if self.similarity_weight_opt.is_some() {
            if let Some(fieldnorm_reader) = reader.fieldnorms_readers().get_field(self.field)? {
                return Ok(fieldnorm_reader);
            }
        }
        Ok(FieldNormReader::constant(reader.max_doc(), 1))
    }

    /// Auto-cascade for a single token: exact → fuzzy → substring → fuzzy substring.
    /// Returns (term_infos, cascade_level) from the first level that finds matches.
    fn cascade_term_infos(
        &self,
        token: &str,
        inverted_index: &InvertedIndexReader,
    ) -> crate::Result<(Vec<TermInfo>, CascadeLevel)> {
        // 1. EXACT: direct term dictionary lookup
        let term = Term::from_field_text(self.field, token);
        if let Some(term_info) = inverted_index.get_term_info(&term)? {
            return Ok((vec![term_info], CascadeLevel::Exact));
        }

        let term_dict = inverted_index.terms();

        // Cached LevenshteinAutomatonBuilder (shared between Fuzzy and FuzzySubstring).
        static AUTOMATON_BUILDER: [[OnceCell<LevenshteinAutomatonBuilder>; 2]; 3] = [
            [OnceCell::new(), OnceCell::new()],
            [OnceCell::new(), OnceCell::new()],
            [OnceCell::new(), OnceCell::new()],
        ];

        // 2. FUZZY: Levenshtein DFA (if enabled and distance ≤ 2)
        if self.fuzzy_distance > 0 && self.fuzzy_distance <= 2 {
            let builder = AUTOMATON_BUILDER[self.fuzzy_distance as usize][1]
                .get_or_init(|| {
                    LevenshteinAutomatonBuilder::new(self.fuzzy_distance, true)
                });
            let dfa = DfaWrapper(builder.build_dfa(token));
            let mut stream = term_dict.search(&dfa).into_stream()?;
            let mut term_infos = Vec::new();
            while stream.advance() {
                term_infos.push(stream.value().clone());
            }
            if !term_infos.is_empty() {
                return Ok((term_infos, CascadeLevel::Fuzzy(self.fuzzy_distance)));
            }
        }

        // 3. SUBSTRING: regex .*{escaped}.*
        let escaped = regex::escape(token);
        let pattern = format!(".*{escaped}.*");
        let regex = Regex::new(&pattern).map_err(|e| {
            crate::LucivyError::InvalidArgument(format!("Invalid contains regex: {e}"))
        })?;
        let mut stream = term_dict.search(&regex).into_stream()?;
        let mut term_infos = Vec::new();
        while stream.advance() {
            term_infos.push(stream.value().clone());
        }
        if !term_infos.is_empty() {
            return Ok((term_infos, CascadeLevel::Substring));
        }

        // 4. FUZZY SUBSTRING: NFA simulation .*{levenshtein(token, d)}.*
        if self.fuzzy_distance > 0 && self.fuzzy_distance <= 2 {
            let builder = AUTOMATON_BUILDER[self.fuzzy_distance as usize][1]
                .get_or_init(|| {
                    LevenshteinAutomatonBuilder::new(self.fuzzy_distance, true)
                });
            let dfa = builder.build_dfa(token);
            let automaton = FuzzySubstringAutomaton::new(dfa);
            let mut stream = term_dict.search(&automaton).into_stream()?;
            let mut term_infos = Vec::new();
            while stream.advance() {
                term_infos.push(stream.value().clone());
            }
            if !term_infos.is_empty() {
                return Ok((term_infos, CascadeLevel::FuzzySubstring(self.fuzzy_distance)));
            }
        }

        // No matches at any level
        Ok((Vec::new(), CascadeLevel::Substring))
    }

    /// Multi-token: cascade per position, then ContainsScorer or PhraseScorer.
    pub(crate) fn phrase_scorer(
        &self,
        reader: &SegmentReader,
        boost: Score,
        segment_id: SegmentId,
    ) -> crate::Result<Option<Box<dyn Scorer>>> {
        let similarity_weight_opt = self
            .similarity_weight_opt
            .as_ref()
            .map(|sw| sw.boost_by(boost));
        let fieldnorm_reader = self.fieldnorm_reader(reader)?;
        let inverted_index = reader.inverted_index(self.field)?;
        let mut posting_lists = Vec::new();
        let mut num_terms = 0;
        let mut cascade_distances = Vec::new();

        for &(offset, ref token) in &self.phrase_terms {
            let (term_infos, level) = self.cascade_term_infos(token, &inverted_index)?;
            if term_infos.is_empty() {
                return Ok(None);
            }
            cascade_distances.push(level.distance());
            num_terms += term_infos.len();
            if num_terms > self.max_expansions as usize {
                return Err(crate::LucivyError::InvalidArgument(format!(
                    "Contains query exceeded max expansions {num_terms}"
                )));
            }
            let union =
                RegexPhraseWeight::get_union_from_term_infos(&term_infos, reader, &inverted_index)?;
            posting_lists.push((offset, union));
        }

        if self.needs_validation() {
            let store_reader = reader
                .get_store_reader(50)
                .map_err(crate::LucivyError::from)?;
            let text_field = self.stored_field.unwrap_or(self.field);
            Ok(Some(Box::new(ContainsScorer::new(
                posting_lists,
                similarity_weight_opt,
                fieldnorm_reader,
                self.query_separators.clone(),
                self.query_prefix.clone(),
                self.query_suffix.clone(),
                self.distance_budget,
                self.strict_separators,
                cascade_distances,
                store_reader,
                text_field,
                self.highlight_sink.clone(),
                self.highlight_field_name.clone(),
                segment_id,
            ))))
        } else {
            Ok(Some(Box::new(PhraseScorer::new(
                posting_lists,
                similarity_weight_opt,
                fieldnorm_reader,
                0, // slop = 0: consecutive positions
            ))))
        }
    }

    /// Single-token: cascade then BitSet scorer or ContainsSingleScorer.
    fn single_token_scorer(
        &self,
        reader: &SegmentReader,
        boost: Score,
        segment_id: SegmentId,
    ) -> crate::Result<Box<dyn Scorer>> {
        let inverted_index = reader.inverted_index(self.field)?;
        let token = &self.phrase_terms[0].1;
        let (term_infos, level) = self.cascade_term_infos(token, &inverted_index)?;
        if term_infos.is_empty() {
            return Ok(Box::new(EmptyScorer));
        }

        let max_doc = reader.max_doc();
        let mut doc_bitset = BitSet::with_max_value(max_doc);
        for term_info in &term_infos {
            let mut block_postings = inverted_index
                .read_block_postings_from_terminfo(term_info, IndexRecordOption::Basic)?;
            loop {
                let docs = block_postings.docs();
                if docs.is_empty() {
                    break;
                }
                for &doc in docs {
                    doc_bitset.insert(doc);
                }
                block_postings.advance();
            }
        }

        if self.needs_validation() {
            let store_reader = reader
                .get_store_reader(50)
                .map_err(crate::LucivyError::from)?;
            let text_field = self.stored_field.unwrap_or(self.field);
            Ok(Box::new(ContainsSingleScorer::new(
                BitSetDocSet::from(doc_bitset),
                store_reader,
                text_field,
                token.clone(),
                self.query_prefix.clone(),
                self.query_suffix.clone(),
                self.distance_budget,
                self.strict_separators,
                level.distance(),
                boost,
                self.highlight_sink.clone(),
                self.highlight_field_name.clone(),
                segment_id,
            )))
        } else {
            Ok(Box::new(ConstScorer::new(
                BitSetDocSet::from(doc_bitset),
                boost,
            )))
        }
    }
}

impl Weight for AutomatonPhraseWeight {
    fn scorer(&self, reader: &SegmentReader, boost: Score) -> crate::Result<Box<dyn Scorer>> {
        let segment_id = reader.segment_id();
        if self.phrase_terms.len() <= 1 {
            return self.single_token_scorer(reader, boost, segment_id);
        }
        if let Some(scorer) = self.phrase_scorer(reader, boost, segment_id)? {
            Ok(scorer)
        } else {
            Ok(Box::new(EmptyScorer))
        }
    }

    fn explain(&self, reader: &SegmentReader, doc: DocId) -> crate::Result<Explanation> {
        let mut scorer = self.scorer(reader, 1.0)?;
        if scorer.seek(doc) != doc {
            return Err(does_not_match(doc));
        }
        Ok(Explanation::new("AutomatonPhraseScorer", scorer.score()))
    }
}

#[cfg(test)]
mod tests {
    use super::super::automaton_phrase_query::AutomatonPhraseQuery;
    use super::super::tests::create_index;
    use crate::docset::TERMINATED;
    use crate::query::{EnableScoring, Weight};
    use crate::DocSet;

    #[test]
    fn test_automaton_phrase_exact() -> crate::Result<()> {
        let index = create_index(&["hello world", "foo bar", "hello there"])?;
        let schema = index.schema();
        let text_field = schema.get_field("text").unwrap();
        let searcher = index.reader()?.searcher();
        let query = AutomatonPhraseQuery::new(
            text_field,
            vec![(0, "hello".into()), (1, "world".into())],
            1000,
            1,
        );
        let weight = query
            .automaton_phrase_weight(EnableScoring::disabled_from_schema(searcher.schema()))?;
        let mut scorer = weight.scorer(searcher.segment_reader(0), 1.0)?;
        assert_eq!(scorer.doc(), 0);
        assert_eq!(scorer.advance(), TERMINATED);
        Ok(())
    }

    #[test]
    fn test_automaton_phrase_fuzzy() -> crate::Result<()> {
        // "helo" is Levenshtein distance 1 from "hello"
        let index = create_index(&["hello world", "foo bar"])?;
        let schema = index.schema();
        let text_field = schema.get_field("text").unwrap();
        let searcher = index.reader()?.searcher();
        let query = AutomatonPhraseQuery::new(
            text_field,
            vec![(0, "helo".into()), (1, "world".into())],
            1000,
            1,
        );
        let weight = query
            .automaton_phrase_weight(EnableScoring::disabled_from_schema(searcher.schema()))?;
        let mut scorer = weight.scorer(searcher.segment_reader(0), 1.0)?;
        assert_eq!(scorer.doc(), 0);
        assert_eq!(scorer.advance(), TERMINATED);
        Ok(())
    }

    #[test]
    fn test_automaton_phrase_substring() -> crate::Result<()> {
        // "ell" is a substring of "hello" → single token, substring regex fallback
        let index = create_index(&["hello world", "foo bar"])?;
        let schema = index.schema();
        let text_field = schema.get_field("text").unwrap();
        let searcher = index.reader()?.searcher();
        let query = AutomatonPhraseQuery::new(text_field, vec![(0, "ell".into())], 1000, 1);
        let weight = query
            .automaton_phrase_weight(EnableScoring::disabled_from_schema(searcher.schema()))?;
        let mut scorer = weight.scorer(searcher.segment_reader(0), 1.0)?;
        // "hello" contains "ell", so doc 0 should match
        assert_eq!(scorer.doc(), 0);
        assert_eq!(scorer.advance(), TERMINATED);
        Ok(())
    }

    #[test]
    fn test_automaton_phrase_no_match() -> crate::Result<()> {
        let index = create_index(&["hello world", "foo bar"])?;
        let schema = index.schema();
        let text_field = schema.get_field("text").unwrap();
        let searcher = index.reader()?.searcher();
        let query = AutomatonPhraseQuery::new(
            text_field,
            vec![(0, "zzz".into()), (1, "qqq".into())],
            1000,
            1,
        );
        let weight = query
            .automaton_phrase_weight(EnableScoring::disabled_from_schema(searcher.schema()))?;
        let mut scorer = weight.scorer(searcher.segment_reader(0), 1.0)?;
        assert_eq!(scorer.doc(), TERMINATED);
        Ok(())
    }

    #[test]
    fn test_automaton_phrase_single_token() -> crate::Result<()> {
        let index = create_index(&["hello world", "foo bar", "hello there"])?;
        let schema = index.schema();
        let text_field = schema.get_field("text").unwrap();
        let searcher = index.reader()?.searcher();
        // Single token exact match — should find docs 0 and 2
        let query = AutomatonPhraseQuery::new(text_field, vec![(0, "hello".into())], 1000, 1);
        let weight = query
            .automaton_phrase_weight(EnableScoring::disabled_from_schema(searcher.schema()))?;
        let mut scorer = weight.scorer(searcher.segment_reader(0), 1.0)?;
        assert_eq!(scorer.doc(), 0);
        assert_eq!(scorer.advance(), 2);
        assert_eq!(scorer.advance(), TERMINATED);
        Ok(())
    }

    #[test]
    fn test_automaton_phrase_fuzzy_substring() -> crate::Result<()> {
        // "progam" (typo for "program") at d=1:
        // - Exact: "progam" not in dict
        // - Fuzzy d=1: "programming" is too far (distance >> 1)
        // - Substring: ".*progam.*" → no term contains "progam" literally
        // - FuzzySubstring: "programming" contains "program" (distance 1 from "progam") → match!
        let index = create_index(&["programming language", "foo bar"])?;
        let schema = index.schema();
        let text_field = schema.get_field("text").unwrap();
        let searcher = index.reader()?.searcher();
        let query =
            AutomatonPhraseQuery::new(text_field, vec![(0, "progam".into())], 1000, 1);
        let weight = query
            .automaton_phrase_weight(EnableScoring::disabled_from_schema(searcher.schema()))?;
        let mut scorer = weight.scorer(searcher.segment_reader(0), 1.0)?;
        assert_eq!(scorer.doc(), 0);
        assert_eq!(scorer.advance(), TERMINATED);
        Ok(())
    }

    #[test]
    fn test_fuzzy_substring_no_false_positive() -> crate::Result<()> {
        // "xyz" at d=1 should not match "programming" (no substring within distance 1)
        let index = create_index(&["programming language", "foo bar"])?;
        let schema = index.schema();
        let text_field = schema.get_field("text").unwrap();
        let searcher = index.reader()?.searcher();
        let query = AutomatonPhraseQuery::new(text_field, vec![(0, "xyz".into())], 1000, 1);
        let weight = query
            .automaton_phrase_weight(EnableScoring::disabled_from_schema(searcher.schema()))?;
        let mut scorer = weight.scorer(searcher.segment_reader(0), 1.0)?;
        assert_eq!(scorer.doc(), TERMINATED);
        Ok(())
    }

    #[test]
    fn test_cascade_early_termination() -> crate::Result<()> {
        // "hello" exists exactly → fuzzy and substring should not be needed.
        // We verify by checking that the exact match returns only 2 docs (not more).
        let index = create_index(&["hello world", "shell game", "hello there"])?;
        let schema = index.schema();
        let text_field = schema.get_field("text").unwrap();
        let searcher = index.reader()?.searcher();
        // "hello" exact match → only docs 0, 2
        // If cascade fell through to substring ".*hello.*", it would still match only "hello"
        // But if it fell to fuzzy, "shell" (distance 2 from "hello") would NOT match at distance 1
        let query = AutomatonPhraseQuery::new(text_field, vec![(0, "hello".into())], 1000, 1);
        let weight = query
            .automaton_phrase_weight(EnableScoring::disabled_from_schema(searcher.schema()))?;
        let mut scorer = weight.scorer(searcher.segment_reader(0), 1.0)?;
        assert_eq!(scorer.doc(), 0);
        assert_eq!(scorer.advance(), 2);
        assert_eq!(scorer.advance(), TERMINATED);
        Ok(())
    }
}
