use std::sync::Arc;

use common::BitSet;
use levenshtein_automata::LevenshteinAutomatonBuilder;
use once_cell::sync::OnceCell;

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
use crate::query::phrase_prefix_query::prefix_end;
use crate::query::{BitSetDocSet, ConstScorer, EmptyScorer, Explanation, Scorer, Weight};
use crate::schema::{Field, IndexRecordOption, Term};
use crate::{DocId, InvertedIndexReader, Score};

/// Cascade level returned by cascade_term_infos.
#[derive(Debug, Clone, Copy)]
pub(crate) enum CascadeLevel {
    Exact,
    Fuzzy(u8),
}

impl CascadeLevel {
    pub fn distance(&self) -> u32 {
        match self {
            CascadeLevel::Exact => 0,
            CascadeLevel::Fuzzy(d) => *d as u32,
        }
    }
}

/// Cached LevenshteinAutomatonBuilder (transposition_cost_one = true).
fn get_automaton_builder(distance: u8) -> &'static LevenshteinAutomatonBuilder {
    static AUTOMATON_BUILDER: [OnceCell<LevenshteinAutomatonBuilder>; 3] = [
        OnceCell::new(),
        OnceCell::new(),
        OnceCell::new(),
    ];
    AUTOMATON_BUILDER[distance as usize]
        .get_or_init(|| LevenshteinAutomatonBuilder::new(distance, true))
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
    last_token_is_prefix: bool,
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
        last_token_is_prefix: bool,
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
            last_token_is_prefix,
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

    /// Auto-cascade for a single token: exact → fuzzy.
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

        // 2. FUZZY: Levenshtein DFA (if enabled and distance ≤ 2)
        if self.fuzzy_distance > 0 && self.fuzzy_distance <= 2 {
            let builder = get_automaton_builder(self.fuzzy_distance);
            let dfa = DfaWrapper(builder.build_dfa(token));
            let mut stream = inverted_index.terms().search(&dfa).into_stream()?;
            let mut term_infos = Vec::new();
            while stream.advance() {
                term_infos.push(stream.value().clone());
            }
            if !term_infos.is_empty() {
                return Ok((term_infos, CascadeLevel::Fuzzy(self.fuzzy_distance)));
            }
        }

        // No matches
        Ok((Vec::new(), CascadeLevel::Exact))
    }

    /// Cascade for a prefix token (last token in startsWith mode):
    /// prefix range → prefix fuzzy DFA.
    fn prefix_term_infos(
        &self,
        token: &str,
        inverted_index: &InvertedIndexReader,
    ) -> crate::Result<(Vec<TermInfo>, CascadeLevel)> {
        let term_dict = inverted_index.terms();

        // 1. PREFIX RANGE: all terms starting with `token`
        let prefix_bytes = token.as_bytes();
        let mut builder = term_dict.range();
        builder = builder.ge(prefix_bytes);
        if let Some(end) = prefix_end(prefix_bytes) {
            builder = builder.lt(&end);
        }
        let mut stream = builder.into_stream()?;
        let mut term_infos = Vec::new();
        while stream.advance() && term_infos.len() < self.max_expansions as usize {
            term_infos.push(stream.value().clone());
        }
        if !term_infos.is_empty() {
            return Ok((term_infos, CascadeLevel::Exact));
        }

        // 2. PREFIX FUZZY: Levenshtein prefix DFA
        if self.fuzzy_distance > 0 && self.fuzzy_distance <= 2 {
            let automaton_builder = get_automaton_builder(self.fuzzy_distance);
            let dfa = DfaWrapper(automaton_builder.build_prefix_dfa(token));
            let mut stream = term_dict.search(&dfa).into_stream()?;
            let mut term_infos = Vec::new();
            while stream.advance() && term_infos.len() < self.max_expansions as usize {
                term_infos.push(stream.value().clone());
            }
            if !term_infos.is_empty() {
                return Ok((term_infos, CascadeLevel::Fuzzy(self.fuzzy_distance)));
            }
        }

        Ok((Vec::new(), CascadeLevel::Exact))
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
        let mut budget = self.max_expansions as usize;
        let mut cascade_distances = Vec::new();

        let last_idx = self.phrase_terms.len() - 1;
        for (i, &(offset, ref token)) in self.phrase_terms.iter().enumerate() {
            let (term_infos, level) = if self.last_token_is_prefix && i == last_idx {
                self.prefix_term_infos(token, &inverted_index)?
            } else {
                self.cascade_term_infos(token, &inverted_index)?
            };
            if term_infos.is_empty() {
                return Ok(None);
            }
            cascade_distances.push(level.distance());
            num_terms += term_infos.len();
            if num_terms > budget {
                // Grant extra headroom for this token, up to once per token.
                budget += 20;
                if num_terms > budget {
                    return Err(crate::LucivyError::InvalidArgument(format!(
                        "Contains query exceeded max expansions ({num_terms} > {budget})"
                    )));
                }
            }
            let union =
                RegexPhraseWeight::get_union_from_term_infos(&term_infos, reader, &inverted_index)?;
            posting_lists.push((offset, union));
        }

        if self.needs_validation() || self.highlight_sink.is_some() {
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
            // Fast path: no validation, no highlights → position-only PhraseScorer.
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
        let (term_infos, level) = if self.last_token_is_prefix {
            self.prefix_term_infos(token, &inverted_index)?
        } else {
            self.cascade_term_infos(token, &inverted_index)?
        };
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

        if self.needs_validation() || self.highlight_sink.is_some() {
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
        let scorer = weight.scorer(searcher.segment_reader(0), 1.0)?;
        assert_eq!(scorer.doc(), TERMINATED);
        Ok(())
    }

    #[test]
    fn test_automaton_phrase_single_token() -> crate::Result<()> {
        let index = create_index(&["hello world", "foo bar", "hello there"])?;
        let schema = index.schema();
        let text_field = schema.get_field("text").unwrap();
        let searcher = index.reader()?.searcher();
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
    fn test_cascade_early_termination() -> crate::Result<()> {
        let index = create_index(&["hello world", "shell game", "hello there"])?;
        let schema = index.schema();
        let text_field = schema.get_field("text").unwrap();
        let searcher = index.reader()?.searcher();
        let query = AutomatonPhraseQuery::new(text_field, vec![(0, "hello".into())], 1000, 1);
        let weight = query
            .automaton_phrase_weight(EnableScoring::disabled_from_schema(searcher.schema()))?;
        let mut scorer = weight.scorer(searcher.segment_reader(0), 1.0)?;
        assert_eq!(scorer.doc(), 0);
        assert_eq!(scorer.advance(), 2);
        assert_eq!(scorer.advance(), TERMINATED);
        Ok(())
    }

    // ─── startsWith tests ───────────────────────────────────────────

    #[test]
    fn test_starts_with_single_token_prefix() -> crate::Result<()> {
        // "hel" should match "hello" via prefix range
        let index = create_index(&["hello world", "foo bar", "help me"])?;
        let schema = index.schema();
        let text_field = schema.get_field("text").unwrap();
        let searcher = index.reader()?.searcher();
        let query = AutomatonPhraseQuery::new_starts_with(
            text_field,
            vec![(0, "hel".into())],
            50,
            0,
        );
        let weight = query
            .automaton_phrase_weight(EnableScoring::disabled_from_schema(searcher.schema()))?;
        let mut scorer = weight.scorer(searcher.segment_reader(0), 1.0)?;
        assert_eq!(scorer.doc(), 0); // "hello"
        assert_eq!(scorer.advance(), 2); // "help"
        assert_eq!(scorer.advance(), TERMINATED);
        Ok(())
    }

    #[test]
    fn test_starts_with_multi_token() -> crate::Result<()> {
        // "hello wor" → ["hello", "wor"] — exact "hello" + prefix "wor" matching "world"
        let index = create_index(&["hello world", "hello work", "hello there"])?;
        let schema = index.schema();
        let text_field = schema.get_field("text").unwrap();
        let searcher = index.reader()?.searcher();
        let query = AutomatonPhraseQuery::new_starts_with(
            text_field,
            vec![(0, "hello".into()), (1, "wor".into())],
            50,
            0,
        );
        let weight = query
            .automaton_phrase_weight(EnableScoring::disabled_from_schema(searcher.schema()))?;
        let mut scorer = weight.scorer(searcher.segment_reader(0), 1.0)?;
        assert_eq!(scorer.doc(), 0); // "hello world"
        assert_eq!(scorer.advance(), 1); // "hello work"
        assert_eq!(scorer.advance(), TERMINATED); // "hello there" — no match
        Ok(())
    }

    #[test]
    fn test_starts_with_fuzzy_prefix() -> crate::Result<()> {
        // "helo" (typo) at distance=1 should match "hello" via fuzzy,
        // then "wor" prefix matches "world"
        let index = create_index(&["hello world", "foo bar"])?;
        let schema = index.schema();
        let text_field = schema.get_field("text").unwrap();
        let searcher = index.reader()?.searcher();
        let query = AutomatonPhraseQuery::new_starts_with(
            text_field,
            vec![(0, "helo".into()), (1, "wor".into())],
            50,
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
    fn test_starts_with_no_substring_fallback() -> crate::Result<()> {
        // "ell" should NOT match "hello" in startsWith mode (it's a substring, not a prefix)
        let index = create_index(&["hello world", "foo bar"])?;
        let schema = index.schema();
        let text_field = schema.get_field("text").unwrap();
        let searcher = index.reader()?.searcher();
        let query = AutomatonPhraseQuery::new_starts_with(
            text_field,
            vec![(0, "ell".into())],
            50,
            0,
        );
        let weight = query
            .automaton_phrase_weight(EnableScoring::disabled_from_schema(searcher.schema()))?;
        let scorer = weight.scorer(searcher.segment_reader(0), 1.0)?;
        assert_eq!(scorer.doc(), TERMINATED);
        Ok(())
    }
}
