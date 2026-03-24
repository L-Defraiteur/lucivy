use std::fmt;
use std::sync::Arc;

use downcast_rs::impl_downcast;

use super::bm25::Bm25StatisticsProvider;
use super::Weight;
use crate::core::searcher::Searcher;
use crate::query::Explanation;
use crate::schema::Schema;
use crate::{DocAddress, Term};

/// Argument used in `Query::weight(..)`
#[derive(Clone)]
pub enum EnableScoring<'a> {
    /// Pass this to enable scoring.
    Enabled {
        /// The searcher (for schema, tokenizers, doc retrieval).
        searcher: &'a Searcher,

        /// Global BM25 statistics provider (Arc for storage in Weights).
        ///
        /// Shared across all queries in a search — same stats whether
        /// local multi-shard or distributed.
        stats: Arc<dyn Bm25StatisticsProvider + Send + Sync>,
    },
    /// Pass this to disable scoring.
    /// This can improve performance.
    Disabled {
        /// Schema is required.
        schema: &'a Schema,
        /// Searcher should be provided if available.
        searcher_opt: Option<&'a Searcher>,
    },
}

impl<'a> EnableScoring<'a> {
    /// Create using [Searcher] with scoring enabled.
    /// The searcher provides both schema/tokenizers AND statistics.
    /// The Searcher is cloned into an Arc for the stats provider (cheap — Arc<Inner>).
    pub fn enabled_from_searcher(searcher: &'a Searcher) -> EnableScoring<'a> {
        EnableScoring::Enabled {
            searcher,
            stats: Arc::new(searcher.clone()),
        }
    }

    /// Create using a custom stats provider with scoring enabled.
    /// Use for multi-shard (AggregatedBm25StatsOwned) or distributed (ExportableStats).
    pub fn enabled_from_statistics_provider(
        stats: Arc<dyn Bm25StatisticsProvider + Send + Sync>,
        searcher: &'a Searcher,
    ) -> EnableScoring<'a> {
        EnableScoring::Enabled { searcher, stats }
    }

    /// Create using [Searcher] with scoring disabled.
    pub fn disabled_from_searcher(searcher: &'a Searcher) -> EnableScoring<'a> {
        EnableScoring::Disabled {
            schema: searcher.schema(),
            searcher_opt: Some(searcher),
        }
    }

    /// Create using [Schema] with scoring disabled.
    pub fn disabled_from_schema(schema: &'a Schema) -> EnableScoring<'a> {
        Self::Disabled {
            schema,
            searcher_opt: None,
        }
    }

    /// Returns the searcher if available.
    pub fn searcher(&self) -> Option<&Searcher> {
        match self {
            EnableScoring::Enabled { searcher, .. } => Some(*searcher),
            EnableScoring::Disabled { searcher_opt, .. } => searcher_opt.to_owned(),
        }
    }

    /// Returns the global stats provider (Arc, storable in Weights).
    pub fn stats(&self) -> Option<&Arc<dyn Bm25StatisticsProvider + Send + Sync>> {
        match self {
            EnableScoring::Enabled { stats, .. } => Some(stats),
            EnableScoring::Disabled { .. } => None,
        }
    }

    /// Returns the schema.
    pub fn schema(&self) -> &Schema {
        match self {
            EnableScoring::Enabled { searcher, .. } => searcher.schema(),
            EnableScoring::Disabled { schema, .. } => schema,
        }
    }

    /// Returns true if the scoring is enabled.
    pub fn is_scoring_enabled(&self) -> bool {
        matches!(self, EnableScoring::Enabled { .. })
    }
}


/// The `Query` trait defines a set of documents and a scoring method
/// for those documents.
///
/// The `Query` trait is in charge of defining :
///
/// - a set of documents
/// - a way to score these documents
///
/// When performing a [search](Searcher::search), these documents will then
/// be pushed to a [`Collector`](crate::collector::Collector),
/// which will in turn be in charge of deciding what to do with them.
///
/// Concretely, this scored docset is represented by the
/// [`Scorer`] trait.
///
/// Because our index is actually split into segments, the
/// query does not actually directly creates [`DocSet`](crate::DocSet) object.
/// Instead, the query creates a [`Weight`] object for a given searcher.
///
/// The weight object, in turn, makes it possible to create
/// a scorer for a specific [`SegmentReader`].
///
/// So to sum it up :
/// - a `Query` is a recipe to define a set of documents as well the way to score them.
/// - a [`Weight`] is this recipe tied to a specific [`Searcher`]. It may for instance hold
///   statistics about the different term of the query. It is created by the query.
/// - a [`Scorer`] is a cursor over the set of matching documents, for a specific [`SegmentReader`].
///   It is created by the [`Weight`].
///
/// When implementing a new type of `Query`, it is normal to implement a
/// dedicated `Query`, [`Weight`] and [`Scorer`].
///
/// [`Scorer`]: crate::query::Scorer
/// [`SegmentReader`]: crate::SegmentReader
pub trait Query: QueryClone + Send + Sync + downcast_rs::Downcast + fmt::Debug {
    /// Create the weight associated with a query.
    ///
    /// If scoring is not required, setting `scoring_enabled` to `false`
    /// can increase performances.
    ///
    /// See [`Weight`].
    fn weight(&self, enable_scoring: EnableScoring<'_>) -> crate::Result<Box<dyn Weight>>;

    /// Returns an `Explanation` for the score of the document.
    fn explain(&self, searcher: &Searcher, doc_address: DocAddress) -> crate::Result<Explanation> {
        let weight = self.weight(EnableScoring::enabled_from_searcher(searcher))?;
        let reader = searcher.segment_reader(doc_address.segment_ord);
        weight.explain(reader, doc_address.doc_id)
    }

    /// Returns the number of documents matching the query.
    fn count(&self, searcher: &Searcher) -> crate::Result<usize> {
        let weight = self.weight(EnableScoring::disabled_from_searcher(searcher))?;
        let mut result = 0;
        for reader in searcher.segment_readers() {
            result += weight.count(reader)? as usize;
        }
        Ok(result)
    }

    /// Returns the number of documents matching the query asynchronously.
    #[cfg(feature = "quickwit")]
    async fn count_async(&self, searcher: &Searcher) -> crate::Result<usize> {
        self.count(searcher)
    }

    /// Extract all of the terms associated with the query and pass them to the
    /// given closure.
    ///
    /// Each term is associated with a boolean indicating whether
    /// positions are required or not.
    ///
    /// Note that there can be multiple instances of any given term
    /// in a query and deduplication must be handled by the visitor.
    fn query_terms<'a>(&'a self, _visitor: &mut dyn FnMut(&'a Term, bool)) {}

    /// Pre-scan segment readers to compute global BM25 statistics.
    ///
    /// Called before `weight()` when cross-segment/shard consistency is needed.
    /// SuffixContainsQuery uses this to compute global doc_freq for correct IDF.
    /// BooleanQuery propagates to sub-queries.
    /// Default: no-op (standard queries get IDF from the statistics provider).
    fn prescan_segments(&mut self, _segments: &[&crate::SegmentReader]) -> crate::Result<()> {
        Ok(())
    }

    /// Collect contains/substring doc_freqs from prescan results.
    /// Used to build ExportableStats for distributed search.
    /// BooleanQuery propagates to sub-queries.
    /// Default: no-op.
    fn collect_prescan_doc_freqs(&self, _out: &mut std::collections::HashMap<String, u64>) {}

    /// Set global doc_freq for contains queries (from coordinator aggregation).
    /// BooleanQuery propagates to sub-queries.
    /// Default: no-op.
    fn set_global_contains_doc_freqs(&mut self, _freqs: &std::collections::HashMap<String, u64>) {}

    /// Extract prescan cache (for moving between DAG nodes).
    /// SuffixContainsQuery moves its cache out. BooleanQuery propagates.
    /// Default: no-op.
    fn take_prescan_cache(
        &mut self,
        _out: &mut std::collections::HashMap<crate::index::SegmentId, crate::query::phrase_query::suffix_contains_query::CachedSfxResult>,
    ) {}

    /// Inject prescan cache (merged from multiple shards by the DAG).
    /// SuffixContainsQuery stores it for scorer(). BooleanQuery propagates.
    /// Default: no-op.
    fn inject_prescan_cache(
        &mut self,
        _cache: std::collections::HashMap<crate::index::SegmentId, crate::query::phrase_query::suffix_contains_query::CachedSfxResult>,
    ) {
    }

    /// Return SFX prescan parameters needed by this query.
    /// Used by the search DAG to run prescan nodes with the exact same
    /// parameters as the query itself — no duplication, no mismatch.
    /// SuffixContainsQuery returns its own params. BooleanQuery aggregates.
    /// Default: empty (no SFX prescan needed).
    fn sfx_prescan_params(&self) -> Vec<SfxPrescanParam> { vec![] }
}

/// Parameters for an SFX prescan walk, extracted from a built query.
/// Ensures the prescan uses the exact same settings as the query's scorer.
#[derive(Clone, Debug)]
pub struct SfxPrescanParam {
    /// The field to prescan.
    pub field: crate::schema::Field,
    /// The query text (lowercased).
    pub query_text: String,
    /// If true, only match SI=0 (prefix/startsWith mode).
    pub prefix_only: bool,
    /// Fuzzy Levenshtein distance (0 = exact).
    pub fuzzy_distance: u8,
    /// If true, use continuation DFA for cross-token matching.
    pub continuation: bool,
}

/// Implements `box_clone`.
pub trait QueryClone {
    /// Returns a boxed clone of `self`.
    fn box_clone(&self) -> Box<dyn Query>;
}

impl<T> QueryClone for T
where T: 'static + Query + Clone
{
    fn box_clone(&self) -> Box<dyn Query> {
        Box::new(self.clone())
    }
}

impl Query for Box<dyn Query> {
    fn weight(&self, enabled_scoring: EnableScoring) -> crate::Result<Box<dyn Weight>> {
        self.as_ref().weight(enabled_scoring)
    }

    fn count(&self, searcher: &Searcher) -> crate::Result<usize> {
        self.as_ref().count(searcher)
    }

    fn query_terms<'a>(&'a self, visitor: &mut dyn FnMut(&'a Term, bool)) {
        self.as_ref().query_terms(visitor);
    }

    fn prescan_segments(&mut self, segments: &[&crate::SegmentReader]) -> crate::Result<()> {
        self.as_mut().prescan_segments(segments)
    }

    fn collect_prescan_doc_freqs(&self, out: &mut std::collections::HashMap<String, u64>) {
        self.as_ref().collect_prescan_doc_freqs(out)
    }

    fn set_global_contains_doc_freqs(&mut self, freqs: &std::collections::HashMap<String, u64>) {
        self.as_mut().set_global_contains_doc_freqs(freqs)
    }

    fn take_prescan_cache(
        &mut self,
        out: &mut std::collections::HashMap<crate::index::SegmentId, crate::query::phrase_query::suffix_contains_query::CachedSfxResult>,
    ) {
        self.as_mut().take_prescan_cache(out)
    }

    fn inject_prescan_cache(
        &mut self,
        cache: std::collections::HashMap<crate::index::SegmentId, crate::query::phrase_query::suffix_contains_query::CachedSfxResult>,
    ) {
        self.as_mut().inject_prescan_cache(cache)
    }

    fn sfx_prescan_params(&self) -> Vec<SfxPrescanParam> {
        self.as_ref().sfx_prescan_params()
    }
}

impl QueryClone for Box<dyn Query> {
    fn box_clone(&self) -> Box<dyn Query> {
        self.as_ref().box_clone()
    }
}

impl_downcast!(Query);
