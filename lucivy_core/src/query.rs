//! Query and schema JSON parsing.
//!
//! Query routing with dual-field layout:
//!   - phrase, parse            → stemmed field (recall: "run" matches "running")
//!   - term, fuzzy, regex       → raw field (precision: exact word forms, lowercase only)
//! The user always references the base field name; routing is transparent.

use std::ops::Bound;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use ld_lucivy::query::{
    AllQuery, BooleanQuery, ContinuationMode, DisjunctionMaxQuery, FuzzyTermQuery,
    HighlightSink, MoreLikeThisQuery, Occur, PhrasePrefixQuery, PhraseQuery, Query,
    QueryParser, RangeQuery, RegexContinuationQuery, RegexQuery, SuffixContainsQuery, TermQuery,
};
use ld_lucivy::schema::OwnedValue;
use ld_lucivy::schema::{Field, FieldType, IndexRecordOption, Schema, Term};
use ld_lucivy::Index;

// ─── Schema Config ──────────────────────────────────────────────────────────

#[derive(Clone, Default, Deserialize, Serialize)]
pub struct SchemaConfig {
    pub fields: Vec<FieldDef>,
    pub tokenizer: Option<String>,
    /// Number of shards for token-aware sharding. None or 1 = no sharding.
    pub shards: Option<usize>,
    /// Only track tokens with global df below this threshold (default 5000).
    /// Higher = more memory, better routing for mid-frequency tokens.
    pub df_threshold: Option<u32>,
    /// Weight for total balance in hybrid shard routing, 0.0-1.0 (default 0.2).
    /// 0.0 = pure per-token routing, 1.0 = pure round-robin-like balance.
    pub balance_weight: Option<f64>,
    /// Build suffix FST (.sfx + .sfxpost) for contains/startsWith queries.
    /// Default: true. Set to false for faster indexation and smaller indexes
    /// when only term/phrase/fuzzy/regex queries are needed.
    pub sfx: Option<bool>,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct FieldDef {
    pub name: String,
    #[serde(rename = "type")]
    pub field_type: String,
    pub stored: Option<bool>,
    pub indexed: Option<bool>,
    pub fast: Option<bool>,
}

// ─── Query Config ───────────────────────────────────────────────────────────

#[derive(Deserialize, Clone)]
pub struct FilterClause {
    pub field: Option<String>,
    pub op: String, // "eq", "ne", "lt", "lte", "gt", "gte", "in", "between", "not_in", "starts_with", "contains", "must", "should", "must_not"
    pub value: Option<serde_json::Value>,
    /// Fuzzy distance for "contains" op (default 1).
    pub distance: Option<u8>,
    /// Sub-clauses for composite ops (must/should/must_not).
    pub clauses: Option<Vec<FilterClause>>,
}

#[derive(Deserialize, Default, Clone)]
pub struct QueryConfig {
    #[serde(rename = "type")]
    pub query_type: String,
    pub field: Option<String>,
    pub fields: Option<Vec<String>>,
    pub value: Option<String>,
    pub terms: Option<Vec<String>>,
    pub pattern: Option<String>,
    pub distance: Option<u8>,
    pub strict_separators: Option<bool>,
    pub regex: Option<bool>,
    // Boolean query sub-clauses
    pub must: Option<Vec<QueryConfig>>,
    pub should: Option<Vec<QueryConfig>>,
    pub must_not: Option<Vec<QueryConfig>>,
    // Filter clauses on non-text fields
    pub filters: Option<Vec<FilterClause>>,
    // DisjunctionMax: sub-queries + tie_breaker
    pub queries: Option<Vec<QueryConfig>>,
    pub tie_breaker: Option<f32>,
    // PhrasePrefixQuery: max_expansions for last-term prefix
    pub max_expansions: Option<u32>,
    // MoreLikeThis: tuning parameters
    pub min_doc_frequency: Option<u64>,
    pub max_doc_frequency: Option<u64>,
    pub min_term_frequency: Option<usize>,
    pub max_query_terms: Option<usize>,
    pub min_word_length: Option<usize>,
    pub max_word_length: Option<usize>,
    pub boost_factor: Option<f32>,
}

// ─── Tokenization Helper ────────────────────────────────────────────────────

/// Tokenize text through the tokenizer configured for a field.
/// Returns the list of tokens (e.g. ["lazy", "dog"] for "lazy dog").
fn tokenize_for_field(index: &Index, field: Field, schema: &Schema, text: &str) -> Vec<String> {
    let tokenizer_name = match schema.get_field_entry(field).field_type() {
        FieldType::Str(opts) => opts
            .get_indexing_options()
            .map(|o| o.tokenizer())
            .unwrap_or("default"),
        _ => "default",
    };

    if let Some(mut tokenizer) = index.tokenizers().get(tokenizer_name) {
        let mut stream = tokenizer.token_stream(text);
        let mut tokens = Vec::new();
        while let Some(token) = stream.next() {
            tokens.push(token.text.clone());
        }
        tokens
    } else {
        // Fallback: just lowercase
        vec![text.to_lowercase()]
    }
}

/// Token with byte offsets, used to extract separators from the query string.
#[allow(dead_code)]
struct TokenWithOffsets {
    text: String,
    offset_from: usize,
    offset_to: usize,
}

/// Tokenize text through the tokenizer configured for a field, preserving byte offsets.
#[allow(dead_code)]
fn tokenize_with_offsets(
    index: &Index,
    field: Field,
    schema: &Schema,
    text: &str,
) -> Vec<TokenWithOffsets> {
    let tokenizer_name = match schema.get_field_entry(field).field_type() {
        FieldType::Str(opts) => opts
            .get_indexing_options()
            .map(|o| o.tokenizer())
            .unwrap_or("default"),
        _ => "default",
    };

    if let Some(mut tokenizer) = index.tokenizers().get(tokenizer_name) {
        let mut stream = tokenizer.token_stream(text);
        let mut tokens = Vec::new();
        while let Some(token) = stream.next() {
            tokens.push(TokenWithOffsets {
                text: token.text.clone(),
                offset_from: token.offset_from,
                offset_to: token.offset_to,
            });
        }
        tokens
    } else {
        vec![TokenWithOffsets {
            text: text.to_lowercase(),
            offset_from: 0,
            offset_to: text.len(),
        }]
    }
}

// ─── Field Resolution ───────────────────────────────────────────────────────

/// Resolve a field by name from the query config.
/// If `use_raw` is true and a `._raw` counterpart exists, use that instead.
fn resolve_field(
    config: &QueryConfig,
    schema: &Schema,
) -> Result<Field, String> {
    let name = config
        .field
        .as_deref()
        .ok_or("query requires 'field'")?;
    schema
        .get_field(name)
        .map_err(|_| format!("unknown field: {name}"))
}

// ─── Split Expansion ────────────────────────────────────────────────────────

/// Returns true if the string contains at least one alphanumeric character.
fn has_alnum(s: &str) -> bool {
    s.chars().any(|c| c.is_alphanumeric())
}

/// Expand a `_split` query into a boolean should of per-word sub-queries.
/// Filters out tokens with no alphanumeric characters (e.g. ":" "—").
/// If only one word remains, returns a single sub-query (no boolean wrapper).
fn expand_split(config: &QueryConfig, sub_type: &str) -> QueryConfig {
    let value = config.value.as_deref().unwrap_or("");
    let words: Vec<&str> = value.split_whitespace().filter(|w| has_alnum(w)).collect();

    if words.len() <= 1 {
        return QueryConfig {
            query_type: sub_type.to_string(),
            field: config.field.clone(),
            value: Some(if words.is_empty() { value.to_string() } else { words[0].to_string() }),
            distance: config.distance,
            ..Default::default()
        };
    }

    let should: Vec<QueryConfig> = words.iter()
        .map(|w| QueryConfig {
            query_type: sub_type.to_string(),
            field: config.field.clone(),
            value: Some(w.to_string()),
            distance: config.distance,
            ..Default::default()
        })
        .collect();

    QueryConfig {
        query_type: "boolean".into(),
        should: Some(should),
        ..Default::default()
    }
}

/// Check that SFX indexing is enabled. Returns an error if sfx_enabled=false.
fn require_sfx(index: &Index) -> Result<(), String> {
    if !index.settings().sfx_enabled {
        return Err(
            "contains/startsWith queries require SFX indexing. \
             Set sfx: true (default) in schema config.".into()
        );
    }
    Ok(())
}

// ─── Query Building ─────────────────────────────────────────────────────────

pub fn build_query(
    config: &QueryConfig,
    schema: &Schema,
    index: &Index,
    highlight_sink: Option<Arc<HighlightSink>>,
) -> Result<Box<dyn Query>, String> {
    let text_query = match config.query_type.as_str() {
        "term" => build_term_query(config, schema, index, highlight_sink),
        "fuzzy" => build_fuzzy_query(config, schema, index, highlight_sink),
        "phrase" => build_phrase_query(config, schema, index, highlight_sink),
        "regex" => build_regex_query(config, schema, highlight_sink),
        "contains" | "sfx_contains" => {
            require_sfx(index)?;
            build_contains_query(config, schema, highlight_sink)
        }
        "startsWith" => {
            require_sfx(index)?;
            build_starts_with_query(config, schema, index, highlight_sink)
        }
        "contains_split" | "sfx_contains_split" => {
            require_sfx(index)?;
            let expanded = expand_split(config, "contains");
            build_query(&expanded, schema, index, highlight_sink)
                .map(|q| q as Box<dyn Query>)
        }
        "startsWith_split" => {
            require_sfx(index)?;
            let expanded = expand_split(config, "startsWith");
            build_query(&expanded, schema, index, highlight_sink)
                .map(|q| q as Box<dyn Query>)
        }
        "boolean" => build_boolean_query(config, schema, index, highlight_sink),
        "parse" => build_parsed_query(config, schema, index),
        "phrase_prefix" => build_phrase_prefix_query(config, schema, index, highlight_sink),
        "disjunction_max" => build_disjunction_max_query(config, schema, index, highlight_sink),
        "more_like_this" => build_more_like_this_query(config, schema),
        other => Err(format!("unknown query type: {other}")),
    }?;

    // Wrap with filter clauses if present.
    if let Some(ref filters) = config.filters {
        if !filters.is_empty() {
            let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
            clauses.push((Occur::Must, text_query));
            for filter in filters {
                clauses.push((Occur::Must, build_filter_clause(filter, schema, index)?));
            }
            return Ok(Box::new(BooleanQuery::new(clauses)));
        }
    }

    Ok(text_query)
}

/// Term query: exact token match on raw field (lowercased only, no stemming).
/// Use `parse` query for stemmed/analyzed search.
fn build_term_query(
    config: &QueryConfig,
    schema: &Schema,
    _index: &Index,
    highlight_sink: Option<Arc<HighlightSink>>,
) -> Result<Box<dyn Query>, String> {
    let field = resolve_field(config, schema)?;
    let value = config.value.as_deref().ok_or("term query requires 'value'")?;

    // Direct lowercase — no tokenizer pipeline, just case-fold for exact token lookup.
    let term = Term::from_field_text(field, &value.to_lowercase());
    // Never use sfxpost for term queries — the standard inverted index provides
    // byte offsets via WithFreqsAndPositionsAndOffsets when highlights are needed.
    // sfxpost resolves ALL docs (~1000ms), standard postings are streamed (~1ms).
    let mut query = TermQuery::new(term, IndexRecordOption::WithFreqs);
    if let Some(sink) = highlight_sink {
        let field_name = config.field.clone().unwrap_or_default();
        query = query.with_highlight_sink(sink, field_name);
    }
    Ok(Box::new(query))
}

/// Fuzzy query: Levenshtein match on raw field (lowercased only, no stemming).
/// Fuzzy query: Levenshtein match on term dict (standard tantivy behavior).
/// Matches individual tokens within edit distance. Fast (term dict DFA walk).
/// For cross-token fuzzy substring search, use contains with distance parameter.
fn build_fuzzy_query(
    config: &QueryConfig,
    schema: &Schema,
    _index: &Index,
    _highlight_sink: Option<Arc<HighlightSink>>,
) -> Result<Box<dyn Query>, String> {
    let field = resolve_field(config, schema)?;
    let value = config.value.as_deref().ok_or("fuzzy query requires 'value'")?;
    let distance = config.distance.unwrap_or(1);

    let term = Term::from_field_text(field, &value.to_lowercase());
    let query = FuzzyTermQuery::new(term, distance, true);
    Ok(Box::new(query))
}

/// Phrase query: tokenize each term through stemmed field, search stemmed index.
fn build_phrase_query(
    config: &QueryConfig,
    schema: &Schema,
    index: &Index,
    highlight_sink: Option<Arc<HighlightSink>>,
) -> Result<Box<dyn Query>, String> {
    let field = resolve_field(config, schema)?;
    let terms_str = config
        .terms
        .as_ref()
        .ok_or("phrase query requires 'terms'")?;

    let terms: Vec<Term> = terms_str
        .iter()
        .map(|t| {
            let tokens = tokenize_for_field(index, field, schema, t);
            let stemmed = tokens.first().map(|s| s.as_str()).unwrap_or(t);
            Term::from_field_text(field, stemmed)
        })
        .collect();

    let mut query = PhraseQuery::new(terms);
    if let Some(sink) = highlight_sink {
        let field_name = config.field.clone().unwrap_or_default();
        query = query.with_highlight_sink(sink, field_name);
    }
    Ok(Box::new(query))
}

/// Contains query: substring search via suffix FST (.sfx file).
/// Zero stored text reads — direct proof via suffix walk + inverted index.
///
/// In regex mode (`regex: true`), uses RegexContinuationQuery for cross-token regex matching.
/// In fuzzy mode (default), uses SuffixContainsQuery with optional Levenshtein distance.
fn build_contains_query(
    config: &QueryConfig,
    schema: &Schema,
    highlight_sink: Option<Arc<HighlightSink>>,
) -> Result<Box<dyn Query>, String> {
    let is_regex = config.regex.unwrap_or(false);
    if is_regex {
        return build_contains_regex(config, schema, highlight_sink);
    }

    let field = resolve_field(config, schema)?;
    let value = config.value.as_deref().ok_or("contains query requires 'value'")?;
    let distance = config.distance.unwrap_or(0);

    // Pass original case — tokenize_query does CamelCaseSplit (needs case) then LowerCaser.
    let mut query = SuffixContainsQuery::new(field, value.to_string())
        .with_fuzzy_distance(distance)
        .with_strict_separators(config.strict_separators.unwrap_or(false));
    if let Some(sink) = highlight_sink {
        let field_name = config.field.clone().unwrap_or_default();
        query = query.with_highlight_sink(sink, field_name);
    }
    Ok(Box::new(query))
}

/// Contains query in regex mode: cross-token regex via RegexContinuationQuery.
fn build_contains_regex(
    config: &QueryConfig,
    schema: &Schema,
    highlight_sink: Option<Arc<HighlightSink>>,
) -> Result<Box<dyn Query>, String> {
    let field = resolve_field(config, schema)?;
    let pattern = config.value.as_deref().ok_or("contains regex query requires 'value'")?;
    let distance = config.distance.unwrap_or(0);

    let mut query = RegexContinuationQuery::from_regex(
        field,
        pattern.to_string(),
        ContinuationMode::Contains,
    );
    query = query.with_fuzzy_distance(distance);
    if let Some(sink) = highlight_sink {
        let field_name = config.field.clone().unwrap_or_default();
        query = query.with_highlight_sink(sink, field_name);
    }
    Ok(Box::new(query))
}

/// StartsWith query: FST prefix search with optional fuzzy.
///
/// Tokenizes the value, then:
/// - Non-last tokens: exact or fuzzy match (full terms, no substring)
/// - Last token: treated as a prefix (FST range or prefix DFA)
/// - All tokens validated at consecutive positions (phrase adjacency)
fn build_starts_with_query(
    config: &QueryConfig,
    schema: &Schema,
    _index: &Index,
    highlight_sink: Option<Arc<HighlightSink>>,
) -> Result<Box<dyn Query>, String> {
    let field = resolve_field(config, schema)?;
    let value = config.value.as_deref().ok_or("startsWith query requires 'value'")?;
    let fuzzy_distance = config.distance.unwrap_or(0);

    let mut query = SuffixContainsQuery::new(field, value.to_lowercase())
        .with_prefix_only()
        .with_fuzzy_distance(fuzzy_distance);
    if let Some(sink) = highlight_sink {
        let field_name = config.field.clone().unwrap_or_default();
        query = query.with_highlight_sink(sink, field_name);
    }
    Ok(Box::new(query))
}

/// Escape regex special characters in a string.
fn regex_escape(s: &str) -> String {
    let mut escaped = String::with_capacity(s.len());
    for c in s.chars() {
        if "\\.*+?()[]{}|^$".contains(c) {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}

/// Regex query: pattern applies to raw field terms (lowercased, not stemmed).
/// Regex query: regex match on term dict (standard tantivy behavior).
/// Matches individual tokens against a regex pattern. Fast (term dict DFA walk).
/// For cross-token regex substring search, use contains with regex=true.
fn build_regex_query(
    config: &QueryConfig,
    schema: &Schema,
    _highlight_sink: Option<Arc<HighlightSink>>,
) -> Result<Box<dyn Query>, String> {
    let field = resolve_field(config, schema)?;
    let pattern = config
        .pattern
        .as_deref()
        .or(config.value.as_deref())
        .ok_or("regex query requires 'pattern' or 'value'")?;

    let query = RegexQuery::from_pattern(pattern, field)
        .map_err(|e| format!("invalid regex: {e}"))?;
    Ok(Box::new(query))
}

fn build_boolean_query(
    config: &QueryConfig,
    schema: &Schema,
    index: &Index,
    highlight_sink: Option<Arc<HighlightSink>>,
) -> Result<Box<dyn Query>, String> {
    let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();

    if let Some(ref must) = config.must {
        for sub in must {
            clauses.push((Occur::Must, build_query(sub, schema, index, highlight_sink.clone())?));
        }
    }
    if let Some(ref should) = config.should {
        for sub in should {
            clauses.push((Occur::Should, build_query(sub, schema, index, highlight_sink.clone())?));
        }
    }
    if let Some(ref must_not) = config.must_not {
        for sub in must_not {
            clauses.push((Occur::MustNot, build_query(sub, schema, index, None)?));
        }
    }

    if clauses.is_empty() {
        return Err("boolean query has no clauses".to_string());
    }

    Ok(Box::new(BooleanQuery::new(clauses)))
}

/// Phrase prefix query: "mutex loc" → matches "mutex lock", "mutex local", etc.
/// Last term is treated as a prefix, preceding terms are exact.
fn build_phrase_prefix_query(
    config: &QueryConfig,
    schema: &Schema,
    index: &Index,
    _highlight_sink: Option<Arc<HighlightSink>>,
) -> Result<Box<dyn Query>, String> {
    let field = resolve_field(config, schema)?;

    // Accept terms=["mutex", "loc"] or value="mutex loc" (split by whitespace)
    let terms_owned: Vec<String>;
    let terms_ref = if let Some(ref terms) = config.terms {
        terms
    } else {
        let value = config.value.as_deref().ok_or("phrase_prefix query requires 'terms' or 'value'")?;
        terms_owned = value.split_whitespace().map(|w| w.to_string()).collect();
        &terms_owned
    };

    if terms_ref.len() < 2 {
        return Err("phrase_prefix query requires at least 2 terms".into());
    }

    let terms: Vec<Term> = terms_ref.iter()
        .map(|t| {
            let tokens = tokenize_for_field(index, field, schema, t);
            let stemmed = tokens.first().map(|s| s.as_str()).unwrap_or(t);
            Term::from_field_text(field, stemmed)
        })
        .collect();

    let mut query = PhrasePrefixQuery::new(terms);
    if let Some(max) = config.max_expansions {
        query.set_max_expansions(max);
    }
    Ok(Box::new(query))
}

/// Disjunction max query: max score among sub-queries, with optional tie_breaker.
/// Useful for multi-field search: best match across fields wins.
fn build_disjunction_max_query(
    config: &QueryConfig,
    schema: &Schema,
    index: &Index,
    highlight_sink: Option<Arc<HighlightSink>>,
) -> Result<Box<dyn Query>, String> {
    let sub_configs = config.queries.as_ref()
        .ok_or("disjunction_max query requires 'queries'")?;

    if sub_configs.is_empty() {
        return Err("disjunction_max query has no sub-queries".into());
    }

    let disjuncts: Vec<Box<dyn Query>> = sub_configs.iter()
        .map(|sub| build_query(sub, schema, index, highlight_sink.clone()))
        .collect::<Result<Vec<_>, _>>()?;

    let tie_breaker = config.tie_breaker.unwrap_or(0.0);
    Ok(Box::new(DisjunctionMaxQuery::with_tie_breaker(disjuncts, tie_breaker)))
}

/// MoreLikeThis query: find documents similar to the given text.
/// Uses value as the reference text, field as the target field.
fn build_more_like_this_query(
    config: &QueryConfig,
    schema: &Schema,
) -> Result<Box<dyn Query>, String> {
    let field = resolve_field(config, schema)?;
    let value = config.value.as_deref()
        .ok_or("more_like_this query requires 'value' (reference text)")?;

    let mut builder = MoreLikeThisQuery::builder();
    if let Some(v) = config.min_doc_frequency { builder = builder.with_min_doc_frequency(v); }
    if let Some(v) = config.max_doc_frequency { builder = builder.with_max_doc_frequency(v); }
    if let Some(v) = config.min_term_frequency { builder = builder.with_min_term_frequency(v); }
    if let Some(v) = config.max_query_terms { builder = builder.with_max_query_terms(v); }
    if let Some(v) = config.min_word_length { builder = builder.with_min_word_length(v); }
    if let Some(v) = config.max_word_length { builder = builder.with_max_word_length(v); }
    if let Some(v) = config.boost_factor { builder = builder.with_boost_factor(v); }

    let doc_fields = vec![(field, vec![OwnedValue::Str(value.to_string())])];
    Ok(Box::new(builder.with_document_fields(doc_fields)))
}

/// Parse query: already uses the field's configured tokenizer (stemmed pipeline).
fn build_parsed_query(
    config: &QueryConfig,
    schema: &Schema,
    index: &Index,
) -> Result<Box<dyn Query>, String> {
    let value = config
        .value
        .as_deref()
        .ok_or("parse query requires 'value'")?;

    let fields: Vec<Field> = if let Some(ref field_names) = config.fields {
        field_names
            .iter()
            .map(|n| {
                schema
                    .get_field(n)
                    .map_err(|_| format!("unknown field: {n}"))
            })
            .collect::<Result<Vec<_>, _>>()?
    } else if let Some(ref field_name) = config.field {
        vec![schema
            .get_field(field_name)
            .map_err(|_| format!("unknown field: {field_name}"))?]
    } else {
        return Err("parse query requires 'field' or 'fields'".to_string());
    };

    let parser = QueryParser::for_index(index, fields);
    parser
        .parse_query(value)
        .map_err(|e| format!("query parse error: {e}"))
}

// ─── Filter Clause Building ────────────────────────────────────────────────

/// Helper: extract a JSON value as the appropriate Term for a given field type.
fn json_to_term(field: Field, field_type: &FieldType, value: &serde_json::Value) -> Result<Term, String> {
    match field_type {
        FieldType::U64(_) => {
            let v = value.as_u64().ok_or_else(|| format!("expected u64 value, got {value}"))?;
            Ok(Term::from_field_u64(field, v))
        }
        FieldType::I64(_) => {
            let v = value.as_i64().ok_or_else(|| format!("expected i64 value, got {value}"))?;
            Ok(Term::from_field_i64(field, v))
        }
        FieldType::F64(_) => {
            let v = value.as_f64().ok_or_else(|| format!("expected f64 value, got {value}"))?;
            Ok(Term::from_field_f64(field, v))
        }
        FieldType::Str(_) => {
            let v = value.as_str().ok_or_else(|| format!("expected string value, got {value}"))?;
            Ok(Term::from_field_text(field, v))
        }
        _ => Err(format!("unsupported field type for filter")),
    }
}

fn build_filter_clause(
    filter: &FilterClause,
    schema: &Schema,
    index: &Index,
) -> Result<Box<dyn Query>, String> {
    // Composite ops (must/should/must_not) — no field required.
    match filter.op.as_str() {
        "must" | "should" | "must_not" => {
            let sub_clauses = filter.clauses.as_ref()
                .ok_or_else(|| format!("'{}' filter requires 'clauses'", filter.op))?;
            let occur = match filter.op.as_str() {
                "must" => Occur::Must,
                "should" => Occur::Should,
                "must_not" => Occur::MustNot,
                _ => unreachable!(),
            };
            let clauses: Vec<(Occur, Box<dyn Query>)> = sub_clauses
                .iter()
                .map(|c| Ok((occur, build_filter_clause(c, schema, index)?)))
                .collect::<Result<Vec<_>, String>>()?;
            if clauses.is_empty() {
                return Err(format!("'{}' filter requires at least one clause", filter.op));
            }
            // MustNot-only BooleanQuery matches nothing — add AllQuery as positive clause.
            if filter.op == "must_not" {
                let mut full_clauses = vec![(Occur::Must, Box::new(AllQuery) as Box<dyn Query>)];
                full_clauses.extend(clauses);
                return Ok(Box::new(BooleanQuery::new(full_clauses)));
            }
            return Ok(Box::new(BooleanQuery::new(clauses)));
        }
        _ => {}
    }

    // Scalar ops — field required.
    let field_name = filter.field.as_deref()
        .ok_or_else(|| format!("'{}' filter requires 'field'", filter.op))?;
    let field = schema
        .get_field(field_name)
        .map_err(|_| format!("unknown filter field: {field_name}"))?;
    let field_type = schema.get_field_entry(field).field_type().clone();

    let value = || filter.value.as_ref()
        .ok_or_else(|| format!("'{}' filter requires 'value'", filter.op));

    match filter.op.as_str() {
        "eq" => {
            let term = json_to_term(field, &field_type, value()?)?;
            Ok(Box::new(TermQuery::new(term, IndexRecordOption::Basic)))
        }
        "ne" => {
            let term = json_to_term(field, &field_type, value()?)?;
            let eq_query = TermQuery::new(term, IndexRecordOption::Basic);
            Ok(Box::new(BooleanQuery::new(vec![
                (Occur::Must, Box::new(AllQuery) as Box<dyn Query>),
                (Occur::MustNot, Box::new(eq_query) as Box<dyn Query>),
            ])))
        }
        "lt" => {
            let term = json_to_term(field, &field_type, value()?)?;
            Ok(Box::new(RangeQuery::new(Bound::Unbounded, Bound::Excluded(term))))
        }
        "lte" => {
            let term = json_to_term(field, &field_type, value()?)?;
            Ok(Box::new(RangeQuery::new(Bound::Unbounded, Bound::Included(term))))
        }
        "gt" => {
            let term = json_to_term(field, &field_type, value()?)?;
            Ok(Box::new(RangeQuery::new(Bound::Excluded(term), Bound::Unbounded)))
        }
        "gte" => {
            let term = json_to_term(field, &field_type, value()?)?;
            Ok(Box::new(RangeQuery::new(Bound::Included(term), Bound::Unbounded)))
        }
        "in" => {
            let values = value()?.as_array().ok_or("'in' operator requires an array value")?;
            let clauses: Vec<(Occur, Box<dyn Query>)> = values
                .iter()
                .map(|v| {
                    let term = json_to_term(field, &field_type, v)?;
                    Ok((Occur::Should, Box::new(TermQuery::new(term, IndexRecordOption::Basic)) as Box<dyn Query>))
                })
                .collect::<Result<Vec<_>, String>>()?;
            if clauses.is_empty() {
                return Err("'in' filter requires at least one value".to_string());
            }
            Ok(Box::new(BooleanQuery::new(clauses)))
        }
        "between" => {
            let arr = value()?.as_array()
                .ok_or("'between' filter requires a [lo, hi] array value")?;
            if arr.len() != 2 {
                return Err("'between' filter requires exactly [lo, hi]".to_string());
            }
            let lo = json_to_term(field, &field_type, &arr[0])?;
            let hi = json_to_term(field, &field_type, &arr[1])?;
            let range_lo = RangeQuery::new(Bound::Included(lo), Bound::Unbounded);
            let range_hi = RangeQuery::new(Bound::Unbounded, Bound::Included(hi));
            Ok(Box::new(BooleanQuery::new(vec![
                (Occur::Must, Box::new(range_lo) as Box<dyn Query>),
                (Occur::Must, Box::new(range_hi) as Box<dyn Query>),
            ])))
        }
        "not_in" => {
            let values = value()?.as_array().ok_or("'not_in' operator requires an array value")?;
            let inner_clauses: Vec<(Occur, Box<dyn Query>)> = values
                .iter()
                .map(|v| {
                    let term = json_to_term(field, &field_type, v)?;
                    Ok((Occur::Should, Box::new(TermQuery::new(term, IndexRecordOption::Basic)) as Box<dyn Query>))
                })
                .collect::<Result<Vec<_>, String>>()?;
            if inner_clauses.is_empty() {
                return Err("'not_in' filter requires at least one value".to_string());
            }
            let in_query = BooleanQuery::new(inner_clauses);
            Ok(Box::new(BooleanQuery::new(vec![
                (Occur::Must, Box::new(AllQuery) as Box<dyn Query>),
                (Occur::MustNot, Box::new(in_query) as Box<dyn Query>),
            ])))
        }
        "starts_with" => {
            let prefix = value()?.as_str()
                .ok_or("'starts_with' filter requires a string value")?;
            let lower = prefix.to_lowercase();
            // Exact match of the prefix itself.
            let exact = TermQuery::new(
                Term::from_field_text(field, &lower),
                IndexRecordOption::Basic,
            );
            // Prefix + at least one more char (.+ avoids the empty-match limitation of .*).
            let escaped = regex_escape(&lower);
            let regex = RegexQuery::from_pattern(&format!("{escaped}.+"), field)
                .map_err(|e| format!("invalid starts_with pattern: {e}"))?;
            Ok(Box::new(BooleanQuery::new(vec![
                (Occur::Should, Box::new(exact) as Box<dyn Query>),
                (Occur::Should, Box::new(regex) as Box<dyn Query>),
            ])))
        }
        "contains" => {
            let substr = value()?.as_str()
                .ok_or("'contains' filter requires a string value")?;
            let distance = filter.distance.unwrap_or(0);
            let config = QueryConfig {
                query_type: "contains".into(),
                field: Some(field_name.to_string()),
                value: Some(substr.to_string()),
                distance: Some(distance),
                ..Default::default()
            };
            build_contains_query(&config, schema, None)
        }
        other => Err(format!("unknown filter operator: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ld_lucivy::schema::{INDEXED, STORED, STRING};
    use serde_json::json;

    // ─── regex_escape ───────────────────────────────────────────────────

    #[test]
    fn test_regex_escape_plain() {
        assert_eq!(regex_escape("hello"), "hello");
    }

    #[test]
    fn test_regex_escape_special_chars() {
        assert_eq!(regex_escape("a.b*c+d"), r"a\.b\*c\+d");
    }

    #[test]
    fn test_regex_escape_all_special() {
        assert_eq!(
            regex_escape(r"()[]{}|^$.*+?\"),
            r"\(\)\[\]\{\}\|\^\$\.\*\+\?\\"
        );
    }

    #[test]
    fn test_regex_escape_empty() {
        assert_eq!(regex_escape(""), "");
    }

    // ─── json_to_term ───────────────────────────────────────────────────

    fn make_test_schema() -> Schema {
        let mut builder = Schema::builder();
        builder.add_u64_field("count", INDEXED | STORED);
        builder.add_i64_field("offset", INDEXED | STORED);
        builder.add_f64_field("score", INDEXED | STORED);
        builder.add_text_field("name", STRING | STORED);
        builder.build()
    }

    #[test]
    fn test_json_to_term_u64() {
        let schema = make_test_schema();
        let field = schema.get_field("count").unwrap();
        let ft = schema.get_field_entry(field).field_type();
        let term = json_to_term(field, ft, &json!(42)).unwrap();
        assert_eq!(term, Term::from_field_u64(field, 42));
    }

    #[test]
    fn test_json_to_term_i64() {
        let schema = make_test_schema();
        let field = schema.get_field("offset").unwrap();
        let ft = schema.get_field_entry(field).field_type();
        let term = json_to_term(field, ft, &json!(-10)).unwrap();
        assert_eq!(term, Term::from_field_i64(field, -10));
    }

    #[test]
    fn test_json_to_term_f64() {
        let schema = make_test_schema();
        let field = schema.get_field("score").unwrap();
        let ft = schema.get_field_entry(field).field_type();
        let term = json_to_term(field, ft, &json!(3.14)).unwrap();
        assert_eq!(term, Term::from_field_f64(field, 3.14));
    }

    #[test]
    fn test_json_to_term_str() {
        let schema = make_test_schema();
        let field = schema.get_field("name").unwrap();
        let ft = schema.get_field_entry(field).field_type();
        let term = json_to_term(field, ft, &json!("hello")).unwrap();
        assert_eq!(term, Term::from_field_text(field, "hello"));
    }

    #[test]
    fn test_json_to_term_type_mismatch() {
        let schema = make_test_schema();
        let field = schema.get_field("count").unwrap();
        let ft = schema.get_field_entry(field).field_type();
        assert!(json_to_term(field, ft, &json!("not a number")).is_err());
    }

    #[test]
    fn test_json_to_term_i64_from_positive() {
        let schema = make_test_schema();
        let field = schema.get_field("offset").unwrap();
        let ft = schema.get_field_entry(field).field_type();
        let term = json_to_term(field, ft, &json!(100)).unwrap();
        assert_eq!(term, Term::from_field_i64(field, 100));
    }

    // ─── QueryConfig deserialization ─────────────────────────────────────

    #[test]
    fn test_query_config_contains() {
        let json = r#"{"type":"contains","field":"body","value":"programming"}"#;
        let config: QueryConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.query_type, "contains");
        assert_eq!(config.field.as_deref(), Some("body"));
        assert_eq!(config.value.as_deref(), Some("programming"));
        assert_eq!(config.regex, None);
        assert_eq!(config.distance, None);
    }

    #[test]
    fn test_query_config_contains_regex() {
        let json = r#"{"type":"contains","field":"body","value":"program[a-z]+","regex":true}"#;
        let config: QueryConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.query_type, "contains");
        assert_eq!(config.regex, Some(true));
    }

    #[test]
    fn test_query_config_contains_hybrid() {
        let json = r#"{"type":"contains","field":"body","value":"program[a-z]+","regex":true,"distance":1}"#;
        let config: QueryConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.regex, Some(true));
        assert_eq!(config.distance, Some(1));
    }

    #[test]
    fn test_query_config_fuzzy() {
        let json = r#"{"type":"fuzzy","field":"body","value":"programing","distance":2}"#;
        let config: QueryConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.query_type, "fuzzy");
        assert_eq!(config.distance, Some(2));
    }

    #[test]
    fn test_query_config_phrase() {
        let json =
            r#"{"type":"phrase","field":"body","terms":["systems","programming"]}"#;
        let config: QueryConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.query_type, "phrase");
        assert_eq!(
            config.terms.as_ref().unwrap(),
            &["systems", "programming"]
        );
    }

    #[test]
    fn test_query_config_with_filters() {
        let json = r#"{"type":"contains","field":"body","value":"rust","filters":[{"field":"count","op":"gte","value":5}]}"#;
        let config: QueryConfig = serde_json::from_str(json).unwrap();
        let filters = config.filters.as_ref().unwrap();
        assert_eq!(filters.len(), 1);
        assert_eq!(filters[0].field.as_deref(), Some("count"));
        assert_eq!(filters[0].op, "gte");
    }

    #[test]
    fn test_query_config_boolean() {
        let json = r#"{"type":"boolean","must":[{"type":"term","field":"body","value":"rust"}],"must_not":[{"type":"term","field":"body","value":"python"}]}"#;
        let config: QueryConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.query_type, "boolean");
        assert_eq!(config.must.as_ref().unwrap().len(), 1);
        assert_eq!(config.must_not.as_ref().unwrap().len(), 1);
        assert!(config.should.is_none());
    }

    // ─── build_filter_clause ────────────────────────────────────────────

    fn make_filter(field: &str, op: &str, value: serde_json::Value) -> FilterClause {
        FilterClause {
            field: Some(field.into()),
            op: op.into(),
            value: Some(value),
            distance: None,
            clauses: None,
        }
    }

    fn make_filter_index() -> (Schema, Index) {
        use ld_lucivy::schema::{TextFieldIndexing, TextOptions};

        let mut builder = Schema::builder();
        builder.add_u64_field("count", INDEXED | STORED);
        builder.add_i64_field("offset", INDEXED | STORED);
        builder.add_f64_field("score", INDEXED | STORED);
        builder.add_text_field("name", STRING | STORED);
        // Raw field for "name" (lowercase only, for contains/fuzzy precision)
        let raw_indexing = TextFieldIndexing::default()
            .set_tokenizer("default")
            .set_index_option(ld_lucivy::schema::IndexRecordOption::WithFreqsAndPositionsAndOffsets);
        builder.add_text_field("name._raw", TextOptions::default().set_indexing_options(raw_indexing));

        let schema = builder.build();
        let index = Index::create_in_ram(schema.clone());

        (schema, index)
    }

    fn assert_filter_ok(filter: &FilterClause) {
        let (schema, index) = make_filter_index();
        assert!(build_filter_clause(filter, &schema, &index).is_ok());
    }

    fn assert_filter_err(filter: &FilterClause) {
        let (schema, index) = make_filter_index();
        assert!(build_filter_clause(filter, &schema, &index).is_err());
    }

    #[test]
    fn test_filter_clause_eq() {
        assert_filter_ok(&make_filter("count", "eq", json!(42)));
    }

    #[test]
    fn test_filter_clause_ne() {
        assert_filter_ok(&make_filter("count", "ne", json!(0)));
    }

    #[test]
    fn test_filter_clause_range_ops() {
        for op in &["lt", "lte", "gt", "gte"] {
            assert_filter_ok(&make_filter("offset", op, json!(100)));
        }
    }

    #[test]
    fn test_filter_clause_in() {
        assert_filter_ok(&make_filter("count", "in", json!([1, 2, 3])));
    }

    #[test]
    fn test_filter_clause_in_empty() {
        assert_filter_err(&make_filter("count", "in", json!([])));
    }

    #[test]
    fn test_filter_clause_unknown_op() {
        assert_filter_err(&make_filter("count", "like", json!("foo")));
    }

    #[test]
    fn test_filter_clause_unknown_field() {
        assert_filter_err(&make_filter("nonexistent", "eq", json!(1)));
    }

    #[test]
    fn test_filter_clause_f64() {
        assert_filter_ok(&make_filter("score", "gte", json!(0.5)));
    }

    #[test]
    fn test_filter_clause_string_eq() {
        assert_filter_ok(&make_filter("name", "eq", json!("hello")));
    }

    // ─── New ops ──────────────────────────────────────────────────────

    #[test]
    fn test_filter_clause_between() {
        assert_filter_ok(&make_filter("offset", "between", json!([-10, 100])));
    }

    #[test]
    fn test_filter_clause_between_bad_arity() {
        assert_filter_err(&make_filter("offset", "between", json!([1])));
        assert_filter_err(&make_filter("offset", "between", json!([1, 2, 3])));
    }

    #[test]
    fn test_filter_clause_not_in() {
        assert_filter_ok(&make_filter("count", "not_in", json!([1, 2, 3])));
    }

    #[test]
    fn test_filter_clause_not_in_empty() {
        assert_filter_err(&make_filter("count", "not_in", json!([])));
    }

    #[test]
    fn test_filter_clause_starts_with() {
        let (schema, index) = make_filter_index();
        let filter = make_filter("name", "starts_with", json!("hel"));
        let result = build_filter_clause(&filter, &schema, &index);
        assert!(result.is_ok(), "starts_with failed: {:?}", result.err());
    }

    #[test]
    fn test_filter_clause_contains() {
        assert_filter_ok(&make_filter("name", "contains", json!("ell")));
    }

    #[test]
    fn test_filter_clause_contains_with_fuzzy() {
        let (schema, index) = make_filter_index();
        let filter = FilterClause {
            field: Some("name".into()),
            op: "contains".into(),
            value: Some(json!("helo")),
            distance: Some(2),
            clauses: None,
        };
        assert!(build_filter_clause(&filter, &schema, &index).is_ok());
    }

    // ─── Composite ops ────────────────────────────────────────────────

    #[test]
    fn test_filter_clause_must_composite() {
        let filter = FilterClause {
            field: None,
            op: "must".into(),
            value: None,
            distance: None,
            clauses: Some(vec![
                make_filter("count", "gte", json!(10)),
                make_filter("score", "lte", json!(0.9)),
            ]),
        };
        assert_filter_ok(&filter);
    }

    #[test]
    fn test_filter_clause_should_composite() {
        let filter = FilterClause {
            field: None,
            op: "should".into(),
            value: None,
            distance: None,
            clauses: Some(vec![
                make_filter("name", "eq", json!("alice")),
                make_filter("name", "eq", json!("bob")),
            ]),
        };
        assert_filter_ok(&filter);
    }

    #[test]
    fn test_filter_clause_must_not_composite() {
        let filter = FilterClause {
            field: None,
            op: "must_not".into(),
            value: None,
            distance: None,
            clauses: Some(vec![
                make_filter("count", "eq", json!(0)),
            ]),
        };
        assert_filter_ok(&filter);
    }

    #[test]
    fn test_filter_clause_composite_empty_clauses() {
        let filter = FilterClause {
            field: None,
            op: "must".into(),
            value: None,
            distance: None,
            clauses: Some(vec![]),
        };
        assert_filter_err(&filter);
    }

    #[test]
    fn test_filter_clause_composite_missing_clauses() {
        let filter = FilterClause {
            field: None,
            op: "must".into(),
            value: None,
            distance: None,
            clauses: None,
        };
        assert_filter_err(&filter);
    }
}
