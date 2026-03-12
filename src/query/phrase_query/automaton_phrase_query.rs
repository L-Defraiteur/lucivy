use std::sync::Arc;

use super::automaton_phrase_weight::AutomatonPhraseWeight;
use super::scoring_utils::HighlightSink;
use crate::query::bm25::Bm25Weight;
use crate::query::{EnableScoring, Query, Weight};
use crate::schema::{Field, IndexRecordOption, Term, Type};

/// `AutomatonPhraseQuery` matches a sequence of tokens with auto-cascade per position:
/// exact → fuzzy (Levenshtein) → substring (regex).
///
/// For multi-token queries, it uses `PhraseScorer` to verify consecutive positions.
/// For single-token queries, it returns a simple doc-set scorer.
///
/// Early termination: as soon as one cascade level finds matches for a position,
/// lower levels are skipped.
#[derive(Clone, Debug)]
pub struct AutomatonPhraseQuery {
    field: Field,
    /// Field used to load stored text for separator validation.
    /// When the index field (e.g. `body._raw`) is not stored, this should point
    /// to the stored counterpart (e.g. `body`).
    stored_field: Option<Field>,
    phrase_terms: Vec<(usize, String)>,
    max_expansions: u32,
    fuzzy_distance: u8,
    query_separators: Vec<String>,
    query_prefix: String,
    query_suffix: String,
    distance_budget: u32,
    strict_separators: bool,
    /// When true, the last token is treated as a prefix (FST range / prefix DFA)
    /// and non-last tokens skip substring cascade levels (exact + fuzzy only).
    last_token_is_prefix: bool,
    highlight_sink: Option<Arc<HighlightSink>>,
    highlight_field_name: String,
}

impl AutomatonPhraseQuery {
    /// Creates a new `AutomatonPhraseQuery`.
    ///
    /// * `field` - The field to search in.
    /// * `phrase_terms` - Pairs of (position_offset, token_text).
    /// * `max_expansions` - Max number of expanded terms across all positions.
    /// * `fuzzy_distance` - Levenshtein distance for the fuzzy cascade level (0 to skip).
    pub fn new(
        field: Field,
        mut phrase_terms: Vec<(usize, String)>,
        max_expansions: u32,
        fuzzy_distance: u8,
    ) -> AutomatonPhraseQuery {
        phrase_terms.sort_by_key(|&(offset, _)| offset);
        AutomatonPhraseQuery {
            field,
            stored_field: None,
            phrase_terms,
            max_expansions,
            fuzzy_distance,
            query_separators: Vec::new(),
            query_prefix: String::new(),
            query_suffix: String::new(),
            distance_budget: 0,
            strict_separators: true,
            last_token_is_prefix: false,
            highlight_sink: None,
            highlight_field_name: String::new(),
        }
    }

    /// Creates a new `AutomatonPhraseQuery` in startsWith mode.
    ///
    /// The last token is treated as a prefix (FST range + prefix DFA).
    /// Non-last tokens use exact + fuzzy only (no substring cascade).
    pub fn new_starts_with(
        field: Field,
        mut phrase_terms: Vec<(usize, String)>,
        max_expansions: u32,
        fuzzy_distance: u8,
    ) -> AutomatonPhraseQuery {
        phrase_terms.sort_by_key(|&(offset, _)| offset);
        AutomatonPhraseQuery {
            field,
            stored_field: None,
            phrase_terms,
            max_expansions,
            fuzzy_distance,
            query_separators: Vec::new(),
            query_prefix: String::new(),
            query_suffix: String::new(),
            distance_budget: 0,
            strict_separators: true,
            last_token_is_prefix: true,
            highlight_sink: None,
            highlight_field_name: String::new(),
        }
    }

    /// Creates a new `AutomatonPhraseQuery` with separator validation.
    ///
    /// * `stored_field` - Field to load stored text from (if different from `field`).
    /// * `query_separators` - Separators between consecutive tokens in the query string.
    /// * `query_prefix` - Characters before the first token in the query string.
    /// * `query_suffix` - Characters after the last token in the query string.
    /// * `distance_budget` - Max cumulative edit distance (fuzzy + separators + prefix/suffix).
    pub fn new_with_separators(
        field: Field,
        stored_field: Option<Field>,
        mut phrase_terms: Vec<(usize, String)>,
        max_expansions: u32,
        fuzzy_distance: u8,
        query_separators: Vec<String>,
        query_prefix: String,
        query_suffix: String,
        distance_budget: u32,
        strict_separators: bool,
    ) -> AutomatonPhraseQuery {
        phrase_terms.sort_by_key(|&(offset, _)| offset);
        AutomatonPhraseQuery {
            field,
            stored_field,
            phrase_terms,
            max_expansions,
            fuzzy_distance,
            query_separators,
            query_prefix,
            query_suffix,
            distance_budget,
            strict_separators,
            last_token_is_prefix: false,
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

    /// The [`Field`] this query targets.
    pub fn field(&self) -> Field {
        self.field
    }

    /// Build the weight, validating schema constraints.
    pub(crate) fn automaton_phrase_weight(
        &self,
        enable_scoring: EnableScoring<'_>,
    ) -> crate::Result<AutomatonPhraseWeight> {
        let schema = enable_scoring.schema();
        let field_entry = schema.get_field_entry(self.field);
        let field_type = field_entry.field_type().value_type();
        if field_type != Type::Str {
            return Err(crate::LucivyError::SchemaError(format!(
                "AutomatonPhraseQuery requires a text field, got {field_type:?}"
            )));
        }

        // Multi-token queries need positions indexed.
        if self.phrase_terms.len() > 1 {
            let has_positions = field_entry
                .field_type()
                .get_index_record_option()
                .map(IndexRecordOption::has_positions)
                .unwrap_or(false);
            if !has_positions {
                let field_name = field_entry.name();
                return Err(crate::LucivyError::SchemaError(format!(
                    "AutomatonPhraseQuery on field {field_name:?} requires positions indexed"
                )));
            }
        }

        let terms: Vec<Term> = self
            .phrase_terms
            .iter()
            .map(|(_, text)| Term::from_field_text(self.field, text))
            .collect();

        let bm25_weight_opt = match enable_scoring {
            EnableScoring::Enabled {
                statistics_provider,
                ..
            } => Some(Bm25Weight::for_terms(statistics_provider, &terms)?),
            EnableScoring::Disabled { .. } => None,
        };

        Ok(AutomatonPhraseWeight::new(
            self.field,
            self.stored_field,
            self.phrase_terms.clone(),
            bm25_weight_opt,
            self.max_expansions,
            self.fuzzy_distance,
            self.query_separators.clone(),
            self.query_prefix.clone(),
            self.query_suffix.clone(),
            self.distance_budget,
            self.strict_separators,
            self.last_token_is_prefix,
            self.highlight_sink.clone(),
            self.highlight_field_name.clone(),
        ))
    }
}

impl Query for AutomatonPhraseQuery {
    fn weight(&self, enable_scoring: EnableScoring<'_>) -> crate::Result<Box<dyn Weight>> {
        let weight = self.automaton_phrase_weight(enable_scoring)?;
        Ok(Box::new(weight))
    }
}
