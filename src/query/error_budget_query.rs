//! **The words of one query under one error budget per document** (lucivy 4.4.2, `contains_split` with a distance;
//! Lucie, 22 November 2026). A fuzzy `contains` scores `-1000 × errors + BM25` (its verified edits, or its missed
//! trigrams). A boolean of them summed their penalties while a word not found cost nothing, so a document that found
//! three words with a typo each ranked below a document that found one word. This query reads every word clause per
//! document — which one found it, its errors, its BM25, its spans — and scores by a budget mode:
//!
//! - `Flat`: `-1000 × (errors of the words found + miss_cost per word missing) + sum of their BM25`;
//! - `Idf`: as `Flat`, a missing word costing `miss_cost × min(1, idf(word) / mean idf)`, the missing words counted in
//!   whole tiers (`floor`): a stop word costs nothing (the bench: « rebuild the vector index » lost its definition,
//!   which has no « the »);
//! - `Window`: as `Flat`, counted in the best window of the document, `4 ×` the query's length in bytes: a word found
//!   elsewhere counts as missing, so a long page scattering the words ranks below a line gathering them;
//! - `Soft`: as `Flat`, with a tier of `SOFT_TIER` (a few BM25 points) instead of 1000: the BM25, normalised by
//!   length, decides earlier.
//!
//! The idf is per segment (the documents of the segment each word matched); the window needs the words' spans, read
//! from one highlight sink per word, and falls back to `Flat` when they are missing.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use super::Scorer;
use crate::docset::TERMINATED;
use crate::index::SegmentReader;
use crate::query::explanation::does_not_match;
use crate::query::{EnableScoring, ErrorBudgetCombiner, Explanation, HighlightSink, Query, Weight};
use crate::{DocId, DocSet, Score, Term};

/// The tier of `BudgetMode::Soft`, in BM25 points.
pub const SOFT_TIER: f64 = 3.0;

/// How an error budget is counted (see the module).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BudgetMode {
    /// `-1000 × (errors of the words found + miss_cost per word missing) + sum of their BM25`.
    Flat,
    /// As `Flat`, a missing word weighted by its idf, the missing words counted in whole tiers.
    Idf,
    /// As `Flat`, counted in the best window of the document.
    Window,
    /// As `Flat`, with tiers of `SOFT_TIER` BM25 points.
    Soft,
}

impl BudgetMode {
    /// `flat`, `idf`, `window`, `soft`; `None` for anything else.
    pub fn parse(name: &str) -> Option<BudgetMode> {
        match name {
            "flat" => Some(BudgetMode::Flat),
            "idf" => Some(BudgetMode::Idf),
            "window" => Some(BudgetMode::Window),
            "soft" => Some(BudgetMode::Soft),
            _ => None,
        }
    }
}

/// One word of the query: its query, the sink its spans go to (for `Window`), and its length in bytes.
#[derive(Debug)]
pub struct BudgetWord {
    /// The query of the word (a `contains`).
    pub query: Box<dyn Query>,
    /// The sink its spans go to, for `BudgetMode::Window`; `None` otherwise.
    pub sink: Option<Arc<HighlightSink>>,
    /// The length of the word, in bytes.
    pub len: usize,
}

/// **The prescan, handed to every sub-query** (as `BooleanQuery` does). Over several shards the search DAG prescans the
/// segments of every shard through the top query: a composite that kept these defaults left its words to prescan
/// themselves in `weight()`, on shard 0 alone, and lost the documents of the other shards (lucivy 4.5.0, 22 November
/// 2026). `$subs` / `$subs_mut` iterate the sub-queries.
macro_rules! forward_prescan_to_subqueries {
    ($subs:ident, $subs_mut:ident) => {
        fn prescan_segments(&mut self, segments: &[&crate::SegmentReader]) -> crate::Result<()> {
            for q in self.$subs_mut() { q.prescan_segments(segments)?; }
            Ok(())
        }
        fn prescan_truncated(&self) -> bool {
            self.$subs().any(|q| q.prescan_truncated())
        }
        fn prescan_segments_more(&mut self, segments: &[&crate::SegmentReader]) -> crate::Result<()> {
            for q in self.$subs_mut() { q.prescan_segments_more(segments)?; }
            Ok(())
        }
        fn collect_prescan_doc_freqs(&self, out: &mut std::collections::HashMap<String, u64>) {
            for q in self.$subs() { q.collect_prescan_doc_freqs(out); }
        }
        fn set_global_contains_doc_freqs(&mut self, freqs: &std::collections::HashMap<String, u64>) {
            for q in self.$subs_mut() { q.set_global_contains_doc_freqs(freqs); }
        }
        fn take_prescan_cache(
            &mut self,
            out: &mut std::collections::HashMap<(String, crate::index::SegmentId), crate::query::phrase_query::suffix_contains_query::CachedSfxResult>,
        ) {
            for q in self.$subs_mut() { q.take_prescan_cache(out); }
        }
        fn inject_prescan_cache(
            &mut self,
            cache: std::collections::HashMap<(String, crate::index::SegmentId), crate::query::phrase_query::suffix_contains_query::CachedSfxResult>,
        ) {
            for q in self.$subs_mut() { q.inject_prescan_cache(cache.clone()); }
        }
        fn sfx_prescan_params(&self) -> Vec<crate::query::SfxPrescanParam> {
            self.$subs().flat_map(|q| q.sfx_prescan_params()).collect()
        }
        fn regex_prescan_params(&self) -> Vec<crate::query::RegexPrescanParam> {
            self.$subs().flat_map(|q| q.regex_prescan_params()).collect()
        }
        fn inject_regex_prescan_cache(
            &mut self,
            cache: std::collections::HashMap<crate::index::SegmentId, crate::query::phrase_query::regex_continuation_query::CachedRegexResult>,
        ) {
            for q in self.$subs_mut() { q.inject_regex_prescan_cache(cache.clone()); }
        }
        fn set_global_regex_doc_freqs(&mut self, freqs: &std::collections::HashMap<String, u64>) {
            for q in self.$subs_mut() { q.set_global_regex_doc_freqs(freqs); }
        }
        fn collect_regex_prescan_doc_freqs(&self, out: &mut std::collections::HashMap<String, u64>) {
            for q in self.$subs() { q.collect_regex_prescan_doc_freqs(out); }
        }
    };
}

/// The words of one query under one error budget per document (see the module).
#[derive(Debug)]
pub struct ErrorBudgetQuery {
    words: Vec<BudgetWord>,
    miss_cost: u32,
    mode: BudgetMode,
}

impl Clone for ErrorBudgetQuery {
    fn clone(&self) -> Self {
        ErrorBudgetQuery {
            words: self.words.iter().map(|w| BudgetWord { query: w.query.box_clone(), sink: w.sink.clone(), len: w.len }).collect(),
            miss_cost: self.miss_cost,
            mode: self.mode,
        }
    }
}

impl ErrorBudgetQuery {
    /// `miss_cost`: the errors a word not found costs (the distance + 1).
    pub fn new(words: Vec<BudgetWord>, miss_cost: u32, mode: BudgetMode) -> ErrorBudgetQuery {
        ErrorBudgetQuery { words, miss_cost, mode }
    }

    fn subqueries(&self) -> impl Iterator<Item = &Box<dyn Query>> {
        self.words.iter().map(|w| &w.query)
    }

    fn subqueries_mut(&mut self) -> impl Iterator<Item = &mut Box<dyn Query>> {
        self.words.iter_mut().map(|w| &mut w.query)
    }
}

impl Query for ErrorBudgetQuery {
    fn weight(&self, enable_scoring: EnableScoring<'_>) -> crate::Result<Box<dyn Weight>> {
        let weights = self.words.iter().map(|w| w.query.weight(enable_scoring.clone())).collect::<crate::Result<Vec<_>>>()?;
        Ok(Box::new(ErrorBudgetWeight {
            weights,
            sinks: self.words.iter().map(|w| w.sink.clone()).collect(),
            query_len: self.words.iter().map(|w| w.len).sum::<usize>() + self.words.len().saturating_sub(1),
            miss_cost: self.miss_cost,
            mode: self.mode,
        }))
    }

    fn query_terms<'a>(&'a self, visitor: &mut dyn FnMut(&'a Term, bool)) {
        for w in &self.words {
            w.query.query_terms(visitor);
        }
    }

    forward_prescan_to_subqueries!(subqueries, subqueries_mut);
}

struct ErrorBudgetWeight {
    weights: Vec<Box<dyn Weight>>,
    sinks: Vec<Option<Arc<HighlightSink>>>,
    query_len: usize,
    miss_cost: u32,
    mode: BudgetMode,
}

/// What one word found in one document: its score, and its spans (start bytes) when known.
type Found = Option<(Score, Vec<usize>)>;

impl ErrorBudgetWeight {
    fn scored(&self, reader: &SegmentReader, boost: Score) -> crate::Result<Vec<(DocId, Score)>> {
        let n = self.weights.len();
        let mut per_doc: BTreeMap<DocId, Vec<Found>> = BTreeMap::new();
        let mut matched = vec![0u32; n];
        for (i, weight) in self.weights.iter().enumerate() {
            let mut scorer = weight.scorer(reader, boost)?;
            let mut doc = scorer.doc();
            while doc != TERMINATED {
                let score = scorer.score();
                let spans: Vec<usize> = self.sinks[i]
                    .as_ref()
                    .and_then(|s| s.get(reader.segment_id(), doc))
                    .map(|by_field| by_field.into_values().flatten().map(|[start, _]| start).collect())
                    .unwrap_or_default();
                per_doc.entry(doc).or_insert_with(|| vec![None; n])[i] = Some((score, spans));
                matched[i] += 1;
                doc = scorer.advance();
            }
        }
        // The weight of a missing word (`Idf`): min(1, idf / mean idf), per segment.
        let total = reader.max_doc().max(1) as f64;
        let idf: Vec<f64> = matched.iter().map(|&m| (1.0 + (total - m as f64 + 0.5) / (m as f64 + 0.5)).ln()).collect();
        let mean = (idf.iter().sum::<f64>() / n.max(1) as f64).max(f64::MIN_POSITIVE);
        let missing_weight: Vec<f64> = idf.iter().map(|w| (w / mean).min(1.0)).collect();
        let window = 4 * self.query_len.max(1);

        Ok(per_doc
            .into_iter()
            .map(|(doc, found)| {
                let in_window = if self.mode == BudgetMode::Window { best_window(&found, window) } else { None };
                let counts = |i: usize| in_window.as_ref().is_none_or(|w| w.contains(&i));
                let (mut errors, mut missing, mut bm25) = (0f64, 0f64, 0f64);
                // `Idf` counts the missing words in whole tiers: a stop word's fraction of a tier vanishes, a rare
                // word still costs its full `miss_cost`.
                for (i, f) in found.iter().enumerate() {
                    match f {
                        Some((score, _)) if counts(i) => {
                            let e = ErrorBudgetCombiner::errors_of(*score);
                            errors += e as f64;
                            bm25 += *score as f64 + 1000.0 * e as f64;
                        }
                        _ => {
                            missing += self.miss_cost as f64 * if self.mode == BudgetMode::Idf { missing_weight[i] } else { 1.0 };
                        }
                    }
                }
                if self.mode == BudgetMode::Idf {
                    missing = missing.floor();
                }
                let tier = if self.mode == BudgetMode::Soft { SOFT_TIER } else { 1000.0 };
                (doc, (-tier * (errors + missing) + bm25) as Score)
            })
            .collect())
    }
}

/// The words found in the best window of `width` bytes: the most words, then the earliest. `None` when no word found
/// has a span (no sink, or one that overflowed): the budget is then counted on the whole document.
fn best_window(found: &[Found], width: usize) -> Option<HashSet<usize>> {
    let mut points: Vec<(usize, usize)> = found
        .iter()
        .enumerate()
        .filter_map(|(i, f)| f.as_ref().map(|(_, spans)| (i, spans)))
        .flat_map(|(i, spans)| spans.iter().map(move |&p| (p, i)))
        .collect();
    if points.is_empty() {
        return None;
    }
    points.sort_unstable();
    let mut best: HashSet<usize> = HashSet::new();
    let mut left = 0;
    for right in 0..points.len() {
        while points[right].0 - points[left].0 > width {
            left += 1;
        }
        let words: HashSet<usize> = points[left..=right].iter().map(|&(_, i)| i).collect();
        if words.len() > best.len() {
            best = words;
        }
    }
    Some(best)
}

impl Weight for ErrorBudgetWeight {
    fn scorer(&self, reader: &SegmentReader, boost: Score) -> crate::Result<Box<dyn Scorer>> {
        Ok(Box::new(ScoredDocs { docs: self.scored(reader, boost)?, cursor: 0 }))
    }

    fn explain(&self, reader: &SegmentReader, doc: DocId) -> crate::Result<Explanation> {
        match self.scored(reader, 1.0)?.into_iter().find(|(d, _)| *d == doc) {
            Some((_, score)) => Ok(Explanation::new("error budget", score)),
            None => Err(does_not_match(doc)),
        }
    }
}

/// Documents and their scores, in document order.
struct ScoredDocs {
    docs: Vec<(DocId, Score)>,
    cursor: usize,
}

impl DocSet for ScoredDocs {
    fn advance(&mut self) -> DocId {
        self.cursor += 1;
        self.doc()
    }

    fn doc(&self) -> DocId {
        self.docs.get(self.cursor).map(|(d, _)| *d).unwrap_or(TERMINATED)
    }

    fn size_hint(&self) -> u32 {
        self.docs.len() as u32
    }
}

impl Scorer for ScoredDocs {
    fn score(&mut self) -> Score {
        self.docs.get(self.cursor).map(|(_, s)| *s).unwrap_or(0.0)
    }
}

/// **One `contains` over several fields under one error budget** (`BestFieldErrorsCombiner`).
#[derive(Debug)]
pub struct BestFieldQuery {
    fields: Vec<Box<dyn Query>>,
}

impl Clone for BestFieldQuery {
    fn clone(&self) -> Self {
        BestFieldQuery { fields: self.fields.iter().map(|q| q.box_clone()).collect() }
    }
}

impl BestFieldQuery {
    /// One query per field, the same `contains` on each.
    pub fn new(fields: Vec<Box<dyn Query>>) -> BestFieldQuery {
        BestFieldQuery { fields }
    }

    fn subqueries(&self) -> impl Iterator<Item = &Box<dyn Query>> {
        self.fields.iter()
    }

    fn subqueries_mut(&mut self) -> impl Iterator<Item = &mut Box<dyn Query>> {
        self.fields.iter_mut()
    }
}

impl Query for BestFieldQuery {
    fn weight(&self, enable_scoring: EnableScoring<'_>) -> crate::Result<Box<dyn Weight>> {
        let fields = self
            .fields
            .iter()
            .map(|q| Ok((crate::query::Occur::Should, q.weight(enable_scoring.clone())?)))
            .collect::<crate::Result<_>>()?;
        Ok(Box::new(crate::query::BooleanWeight::new(
            fields,
            enable_scoring.is_scoring_enabled(),
            Box::new(crate::query::BestFieldErrorsCombiner::default),
        )))
    }

    fn query_terms<'a>(&'a self, visitor: &mut dyn FnMut(&'a Term, bool)) {
        for q in &self.fields {
            q.query_terms(visitor);
        }
    }

    forward_prescan_to_subqueries!(subqueries, subqueries_mut);
}
