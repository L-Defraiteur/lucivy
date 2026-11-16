//! The entry used by the real derived-in-RAM consumer and its lightweight witness.
//! Loading, decoding and panel coverage precede every index-building callback.

use lucivy_core::query::QueryConfig;
use super::corpus_oracle::{self, OracleError, Span};

pub type LoadedCorpus = Vec<(String, Vec<u8>)>;
pub type Corpus = Vec<(String, String)>;

#[derive(Debug, PartialEq)]
pub enum FixtureError {
    Read { source: String, reason: String },
    Utf8 { source: String, reason: String },
    Query { query: String, reason: String },
    UncoveredPanel { queries: Vec<String> },
}

pub struct PanelQuery {
    pub label: &'static str,
    pub config: QueryConfig,
}

/// The declared contains(content) panel modes, evaluated without an index or I/O.
/// Fuzzy shares the occurrence definition of the existing engine oracle.
pub struct PreparedQuery<'a> {
    config: &'a QueryConfig,
    needle: &'a str,
    regex: Option<regex::Regex>,
}

impl<'a> PreparedQuery<'a> {
    pub fn new(config: &'a QueryConfig) -> Result<Self, FixtureError> {
        let needle = config.value.as_deref().ok_or_else(|| FixtureError::Query {
            query: "<missing value>".into(), reason: "the fixture requires a query value".into(),
        })?;
        let error = |reason: String| FixtureError::Query { query: needle.into(), reason };
        if config.query_type != "contains" || config.field.as_deref() != Some("content") {
            return Err(error("the fixture requires contains(content) queries".into()));
        }
        if config.fuzzy_metric.as_deref().is_some_and(|metric| metric != "levenshtein")
            || config.min_similarity.is_some()
        {
            return Err(error("the fixture panel does not declare this fuzzy metric".into()));
        }
        let regex = if config.regex.unwrap_or(false) {
            Some(regex::RegexBuilder::new(needle).case_insensitive(true).build()
                .map_err(|cause| error(format!("invalid regex pattern: {cause}")))?)
        } else { None };
        Ok(Self { config, needle, regex })
    }

    pub fn spans(&self, source: &str, bytes: &[u8]) -> Result<Vec<Span>, FixtureError> {
        let error = |cause: OracleError| match cause {
            OracleError::Utf8(cause) => FixtureError::Utf8 { source: source.into(), reason: cause.to_string() },
            cause => FixtureError::Query {
                query: self.needle.into(), reason: format!("oracle failed for {source}: {cause:?}"),
            },
        };
        let spans = if let Some(regex) = &self.regex {
            corpus_oracle::regex_spans(bytes, regex)
        } else if self.config.distance.unwrap_or(0) > 0 {
            corpus_oracle::fuzzy_spans(bytes, self.needle, self.config.distance.unwrap_or(0))
        } else {
            corpus_oracle::literal_spans(bytes, self.needle, self.config.strict_separators.unwrap_or(false))
        }.map_err(&error)?;
        corpus_oracle::filter_boundaries(bytes, &spans,
            self.config.anchor_start.unwrap_or(false), self.config.exact_match.unwrap_or(false))
            .map_err(error)
    }
}

pub fn panel() -> Vec<PanelQuery> {
    let q = |text: &str, label, strict, distance, anchor, exact| PanelQuery {
        label,
        config: QueryConfig {
            query_type: "contains".into(), field: Some("content".into()), value: Some(text.into()),
            strict_separators: Some(strict),
            distance: if distance > 0 && distance != 255 { Some(distance) } else { None },
            regex: if distance == 255 { Some(true) } else { None },
            anchor_start: if anchor { Some(true) } else { None },
            exact_match: if exact { Some(true) } else { None },
            ..Default::default()
        },
    };
    vec![
        q("mutex_lock", "strict", true, 0, false, false),
        q("mutex lock", "relax", false, 0, false, false),
        q("spin_lock", "strict", true, 0, false, false),
        q("sched", "term", true, 0, true, true),
        q("printk", "sw", true, 0, true, false),
        q("return", "relax", false, 0, false, false),
        q("if (", "strict", true, 0, false, false),
        q("schdule", "fz1", false, 1, false, false),
        q("regsiter", "fz2", false, 2, false, false),
        q("spin_lock_[a-z]+", "rx", true, 255, false, false),
        q("Mutex", "relax-case", false, 0, false, false),
    ]
}

pub fn with_derived_fixture<T>(
    load: impl FnOnce() -> Result<LoadedCorpus, FixtureError>,
    build: impl FnOnce(&Corpus, &[PanelQuery]) -> T,
) -> Result<T, FixtureError> {
    let files = load()?.into_iter().map(|(source, bytes)| {
        String::from_utf8(bytes).map(|text| (source.clone(), text))
            .map_err(|error| FixtureError::Utf8 { source, reason: error.to_string() })
    }).collect::<Result<Corpus, _>>()?;
    let panel = panel();
    let prepared = panel.iter().map(|query| PreparedQuery::new(&query.config))
        .collect::<Result<Vec<_>, _>>()?;
    let mut missing = Vec::new();
    for (query, matcher) in panel.iter().zip(&prepared) {
        let mut covered = false;
        for (source, text) in &files {
            if !matcher.spans(source, text.as_bytes())?.is_empty() {
                covered = true;
                break;
            }
        }
        if !covered { missing.push(format!("{}: {}", query.label, matcher.needle)); }
    }
    if !missing.is_empty() { return Err(FixtureError::UncoveredPanel { queries: missing }); }
    Ok(build(&files, &panel))
}
