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
    AllQuery, AutomatonPhraseQuery, BooleanQuery, FuzzyParams, FuzzyTermQuery, HighlightSink,
    NgramContainsQuery, Occur, PhraseQuery, Query, QueryParser, RangeQuery, RegexParams,
    RegexQuery, TermQuery, VerificationMode,
};
use regex::Regex;
use regex_syntax::hir::literal::Extractor;
use ld_lucivy::schema::{Field, FieldType, IndexRecordOption, Schema, Term};
use ld_lucivy::Index;

// ─── Schema Config ──────────────────────────────────────────────────────────

#[derive(Clone, Deserialize, Serialize)]
pub struct SchemaConfig {
    pub fields: Vec<FieldDef>,
    pub tokenizer: Option<String>,
    pub stemmer: Option<String>,
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

#[derive(Deserialize)]
pub struct FilterClause {
    pub field: Option<String>,
    pub op: String, // "eq", "ne", "lt", "lte", "gt", "gte", "in", "between", "not_in", "starts_with", "contains", "must", "should", "must_not"
    pub value: Option<serde_json::Value>,
    /// Fuzzy distance for "contains" op (default 1).
    pub distance: Option<u8>,
    /// Sub-clauses for composite ops (must/should/must_not).
    pub clauses: Option<Vec<FilterClause>>,
}

#[derive(Deserialize, Default)]
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
}

// ─── Tokenization Helper ────────────────────────────────────────────────────

/// Tokenize text through the tokenizer configured for a field.
/// Returns the list of tokens (e.g. ["lazi", "dog"] for "lazy dog" with English stemmer).
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
struct TokenWithOffsets {
    text: String,
    offset_from: usize,
    offset_to: usize,
}

/// Tokenize text through the tokenizer configured for a field, preserving byte offsets.
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
    raw_pairs: &[(String, String)],
    use_raw: bool,
) -> Result<Field, String> {
    let name = config
        .field
        .as_deref()
        .ok_or("query requires 'field'")?;

    let actual_name = if use_raw {
        raw_pairs
            .iter()
            .find(|(user, _)| user == name)
            .map(|(_, raw)| raw.as_str())
            .unwrap_or(name) // no raw counterpart → use base field
    } else {
        name
    };

    schema
        .get_field(actual_name)
        .map_err(|_| format!("unknown field: {actual_name}"))
}

// ─── Query Building ─────────────────────────────────────────────────────────

pub fn build_query(
    config: &QueryConfig,
    schema: &Schema,
    index: &Index,
    raw_pairs: &[(String, String)],
    ngram_pairs: &[(String, String)],
    highlight_sink: Option<Arc<HighlightSink>>,
) -> Result<Box<dyn Query>, String> {
    let text_query = match config.query_type.as_str() {
        "term" => build_term_query(config, schema, index, raw_pairs, highlight_sink),
        "fuzzy" => build_fuzzy_query(config, schema, index, raw_pairs, highlight_sink),
        "phrase" => build_phrase_query(config, schema, index, raw_pairs, highlight_sink),
        "regex" => build_regex_query(config, schema, raw_pairs, highlight_sink),
        "contains" => {
            build_contains_query(config, schema, index, raw_pairs, ngram_pairs, highlight_sink)
        }
        "startsWith" => build_starts_with_query(config, schema, index, raw_pairs, highlight_sink),
        "boolean" => build_boolean_query(config, schema, index, raw_pairs, ngram_pairs, highlight_sink),
        "parse" => build_parsed_query(config, schema, index),
        other => Err(format!("unknown query type: {other}")),
    }?;

    // Wrap with filter clauses if present.
    if let Some(ref filters) = config.filters {
        if !filters.is_empty() {
            let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
            clauses.push((Occur::Must, text_query));
            for filter in filters {
                clauses.push((Occur::Must, build_filter_clause(filter, schema, index, raw_pairs, ngram_pairs)?));
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
    raw_pairs: &[(String, String)],
    highlight_sink: Option<Arc<HighlightSink>>,
) -> Result<Box<dyn Query>, String> {
    let field = resolve_field(config, schema, raw_pairs, true)?;
    let value = config.value.as_deref().ok_or("term query requires 'value'")?;

    // Direct lowercase — no tokenizer pipeline, just case-fold for exact token lookup.
    let term = Term::from_field_text(field, &value.to_lowercase());
    let mut query = TermQuery::new(term, IndexRecordOption::WithFreqs);
    if let Some(sink) = highlight_sink {
        let field_name = config.field.clone().unwrap_or_default();
        query = query.with_highlight_sink(sink, field_name);
    }
    Ok(Box::new(query))
}

/// Fuzzy query: Levenshtein match on raw field (lowercased only, no stemming).
fn build_fuzzy_query(
    config: &QueryConfig,
    schema: &Schema,
    _index: &Index,
    raw_pairs: &[(String, String)],
    highlight_sink: Option<Arc<HighlightSink>>,
) -> Result<Box<dyn Query>, String> {
    let field = resolve_field(config, schema, raw_pairs, true)?;
    let value = config.value.as_deref().ok_or("fuzzy query requires 'value'")?;
    let distance = config.distance.unwrap_or(1);

    // Direct lowercase — Levenshtein automaton runs on the raw token form.
    let term = Term::from_field_text(field, &value.to_lowercase());
    let mut query = FuzzyTermQuery::new(term, distance, true);
    if let Some(sink) = highlight_sink {
        let field_name = config.field.clone().unwrap_or_default();
        query = query.with_highlight_sink(sink, field_name);
    }
    Ok(Box::new(query))
}

/// Phrase query: tokenize each term through stemmed field, search stemmed index.
fn build_phrase_query(
    config: &QueryConfig,
    schema: &Schema,
    index: &Index,
    raw_pairs: &[(String, String)],
    highlight_sink: Option<Arc<HighlightSink>>,
) -> Result<Box<dyn Query>, String> {
    let field = resolve_field(config, schema, raw_pairs, false)?;
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

/// Contains query: auto-cascade per position (exact → fuzzy → substring).
///   - Uses AutomatonPhraseQuery which handles both single-token and multi-token.
///   - Fuzzy distance defaults to 1 (configurable via `distance` field).
///   - Extracts separators, prefix, and suffix from the query string for validation.
/// Resolve the ngram field for a user field name, if ngram pairs are configured.
fn resolve_ngram_field(
    config: &QueryConfig,
    schema: &Schema,
    ngram_pairs: &[(String, String)],
) -> Option<Field> {
    let name = config.field.as_deref()?;
    let ngram_name = ngram_pairs
        .iter()
        .find(|(user, _)| user == name)
        .map(|(_, ngram)| ngram.as_str())?;
    schema.get_field(ngram_name).ok()
}

fn build_contains_query(
    config: &QueryConfig,
    schema: &Schema,
    index: &Index,
    raw_pairs: &[(String, String)],
    ngram_pairs: &[(String, String)],
    highlight_sink: Option<Arc<HighlightSink>>,
) -> Result<Box<dyn Query>, String> {
    let is_regex = config.regex.unwrap_or(false);
    if is_regex {
        return build_contains_regex(config, schema, raw_pairs, ngram_pairs, highlight_sink);
    }
    build_contains_fuzzy(config, schema, index, raw_pairs, ngram_pairs, highlight_sink)
}

/// Contains query in fuzzy mode (default): tokenize → trigrams → fuzzy verification → BM25.
fn build_contains_fuzzy(
    config: &QueryConfig,
    schema: &Schema,
    index: &Index,
    raw_pairs: &[(String, String)],
    ngram_pairs: &[(String, String)],
    highlight_sink: Option<Arc<HighlightSink>>,
) -> Result<Box<dyn Query>, String> {
    let field = resolve_field(config, schema, raw_pairs, true)?;
    let stored_field = resolve_field(config, schema, raw_pairs, false).ok();
    let value = config.value.as_deref().ok_or("contains query requires 'value'")?;
    let fuzzy_distance = config.distance.unwrap_or(1);
    let strict_separators = config.strict_separators.unwrap_or(true);


    let tokens = tokenize_with_offsets(index, field, schema, value);


    if tokens.is_empty() {
        let escaped = regex_escape(&value.to_lowercase());
        let pattern = format!(".*{escaped}.*");
        return RegexQuery::from_pattern(&pattern, field)
            .map(|q| Box::new(q) as Box<dyn Query>)
            .map_err(|e| format!("invalid contains pattern: {e}"));
    }

    let separators: Vec<String> = tokens
        .windows(2)
        .map(|w| value[w[0].offset_to..w[1].offset_from].to_string())
        .collect();

    let prefix = value[..tokens.first().map(|t| t.offset_from).unwrap_or(0)].to_string();
    let suffix = value[tokens.last().map(|t| t.offset_to).unwrap_or(value.len())..].to_string();

    let token_texts: Vec<String> = tokens.iter().map(|t| t.text.clone()).collect();
    let distance_budget = fuzzy_distance as u32;

    let ngram_field = resolve_ngram_field(config, schema, ngram_pairs)
        .ok_or_else(|| {
            let name = config.field.as_deref().unwrap_or("?");
            format!(
                "contains query requires an ngram field for '{name}'. \
                 Configure a _ngram field in your schema for substring search."
            )
        })?;

    let verification = VerificationMode::Fuzzy(FuzzyParams {
        tokens: token_texts.clone(),
        separators,
        prefix,
        suffix,
        fuzzy_distance,
        distance_budget,
        strict_separators,
    });
    let mut query = NgramContainsQuery::new(
        field,
        ngram_field,
        stored_field,
        token_texts,
        verification,
    );
    if let Some(sink) = highlight_sink {
        query = query.with_highlight_sink(sink, config.field.clone().unwrap_or_default());
    }
    Ok(Box::new(query))
}

/// Contains query in regex mode: parse pattern → extract literals → trigrams → regex verification → BM25.
fn build_contains_regex(
    config: &QueryConfig,
    schema: &Schema,
    raw_pairs: &[(String, String)],
    ngram_pairs: &[(String, String)],
    highlight_sink: Option<Arc<HighlightSink>>,
) -> Result<Box<dyn Query>, String> {
    let field = resolve_field(config, schema, raw_pairs, true)?;
    let stored_field = resolve_field(config, schema, raw_pairs, false).ok();
    let pattern = config.value.as_deref().ok_or("contains regex query requires 'value'")?;
    let fuzzy_distance = config.distance.unwrap_or(0); // regex default: no fuzzy

    // 1. Fold the pattern to ASCII (ç→c, é→e) for accent-insensitive matching.
    //    The scorer folds stored text before running the regex, so the pattern must also be folded.
    let mut folded_pattern = String::new();
    ld_lucivy::tokenizer::to_ascii(pattern, &mut folded_pattern);
    let compiled = Regex::new(&format!("(?i){folded_pattern}"))
        .map_err(|e| format!("invalid regex pattern: {e}"))?;

    // 2. Parse HIR and extract obligatory literals.
    let hir = regex_syntax::parse(pattern)
        .map_err(|e| format!("invalid regex syntax: {e}"))?;
    let seq = Extractor::new().extract(&hir);
    let literals: Vec<String> = seq
        .literals()
        .map(|lits| {
            lits.iter()
                .map(|lit| String::from_utf8_lossy(lit.as_bytes()).to_lowercase())
                .filter(|s| s.len() >= 3) // Only keep literals >= 3 chars (useful for trigrams)
                .collect()
        })
        .unwrap_or_default();

    // 3. If we have an ngram field, always use NgramContainsQuery (with or without literals).
    //    When literals are empty (< 3 chars), the scorer does a full segment scan
    //    instead of trigram-based candidate collection, but still uses BM25 scoring.
    if let Some(ngram_field) = resolve_ngram_field(config, schema, ngram_pairs) {
        let verification = VerificationMode::Regex(RegexParams {
            compiled,
            literals: literals.clone(),
            fuzzy_distance,
        });
        let mut query = NgramContainsQuery::new(
            field,
            ngram_field,
            stored_field,
            literals,
            verification,
        );
        if let Some(sink) = highlight_sink {
            query = query.with_highlight_sink(sink, config.field.clone().unwrap_or_default());
        }
        return Ok(Box::new(query));
    }

    // Fallback (no ngram field): standard RegexQuery (FST walk, ConstScorer — no BM25).
    let mut query = RegexQuery::from_pattern(&format!("(?i){pattern}"), field)
        .map_err(|e| format!("invalid regex: {e}"))?;
    if let Some(sink) = highlight_sink {
        query = query.with_highlight_sink(sink, config.field.clone().unwrap_or_default());
    }
    Ok(Box::new(query) as Box<dyn Query>)
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
    index: &Index,
    raw_pairs: &[(String, String)],
    highlight_sink: Option<Arc<HighlightSink>>,
) -> Result<Box<dyn Query>, String> {
    let field = resolve_field(config, schema, raw_pairs, true)?;
    let value = config.value.as_deref().ok_or("startsWith query requires 'value'")?;
    let fuzzy_distance = config.distance.unwrap_or(0);

    let tokens = tokenize_for_field(index, field, schema, value);
    if tokens.is_empty() {
        return Err("startsWith query produced no tokens".into());
    }

    // Single token: use FuzzyTermQuery in prefix mode directly.
    if tokens.len() == 1 {
        let term = Term::from_field_text(field, &tokens[0]);
        let mut query = FuzzyTermQuery::new_prefix(term, fuzzy_distance, true);
        if let Some(sink) = highlight_sink {
            query = query.with_highlight_sink(sink, config.field.clone().unwrap_or_default());
        }
        return Ok(Box::new(query));
    }

    // Multi-token: AutomatonPhraseQuery in startsWith mode.
    let phrase_terms: Vec<(usize, String)> = tokens.into_iter().enumerate().collect();
    let mut query = AutomatonPhraseQuery::new_starts_with(
        field,
        phrase_terms,
        50, // max_expansions for prefix
        fuzzy_distance,
    );
    if let Some(sink) = highlight_sink {
        query = query.with_highlight_sink(sink, config.field.clone().unwrap_or_default());
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
fn build_regex_query(
    config: &QueryConfig,
    schema: &Schema,
    raw_pairs: &[(String, String)],
    highlight_sink: Option<Arc<HighlightSink>>,
) -> Result<Box<dyn Query>, String> {
    let field = resolve_field(config, schema, raw_pairs, true)?;
    let pattern = config
        .pattern
        .as_deref()
        .ok_or("regex query requires 'pattern'")?;
    let mut query = RegexQuery::from_pattern(pattern, field)
        .map_err(|e| format!("invalid regex: {e}"))?;
    if let Some(sink) = highlight_sink {
        query = query.with_highlight_sink(sink, config.field.clone().unwrap_or_default());
    }
    Ok(Box::new(query) as Box<dyn Query>)
}

fn build_boolean_query(
    config: &QueryConfig,
    schema: &Schema,
    index: &Index,
    raw_pairs: &[(String, String)],
    ngram_pairs: &[(String, String)],
    highlight_sink: Option<Arc<HighlightSink>>,
) -> Result<Box<dyn Query>, String> {
    let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();

    if let Some(ref must) = config.must {
        for sub in must {
            clauses.push((Occur::Must, build_query(sub, schema, index, raw_pairs, ngram_pairs, highlight_sink.clone())?));
        }
    }
    if let Some(ref should) = config.should {
        for sub in should {
            clauses.push((Occur::Should, build_query(sub, schema, index, raw_pairs, ngram_pairs, highlight_sink.clone())?));
        }
    }
    if let Some(ref must_not) = config.must_not {
        for sub in must_not {
            clauses.push((Occur::MustNot, build_query(sub, schema, index, raw_pairs, ngram_pairs, None)?));
        }
    }

    if clauses.is_empty() {
        return Err("boolean query has no clauses".to_string());
    }

    Ok(Box::new(BooleanQuery::new(clauses)))
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
    raw_pairs: &[(String, String)],
    ngram_pairs: &[(String, String)],
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
                .map(|c| Ok((occur, build_filter_clause(c, schema, index, raw_pairs, ngram_pairs)?)))
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
            let distance = filter.distance.unwrap_or(1);
            let config = QueryConfig {
                query_type: "contains".into(),
                field: Some(field_name.to_string()),
                value: Some(substr.to_string()),
                distance: Some(distance),
                ..Default::default()
            };
            build_contains_query(&config, schema, index, raw_pairs, ngram_pairs, None)
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
        let schema = make_test_schema();
        let index = Index::create_in_ram(schema.clone());
        (schema, index)
    }

    fn assert_filter_ok(filter: &FilterClause) {
        let (schema, index) = make_filter_index();
        assert!(build_filter_clause(filter, &schema, &index, &[], &[]).is_ok());
    }

    fn assert_filter_err(filter: &FilterClause) {
        let (schema, index) = make_filter_index();
        assert!(build_filter_clause(filter, &schema, &index, &[], &[]).is_err());
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
        let result = build_filter_clause(&filter, &schema, &index, &[], &[]);
        assert!(result.is_ok(), "starts_with failed: {:?}", result.err());
    }

    #[test]
    fn test_filter_clause_contains() {
        // Contains dispatches to build_contains_query — needs an index with the field.
        // With no ngram/raw pairs, falls back to AutomatonPhraseQuery or RegexQuery.
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
        assert!(build_filter_clause(&filter, &schema, &index, &[], &[]).is_ok());
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
