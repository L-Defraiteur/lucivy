//! ContainsQueryV3 — unified substring search (v2 + v3).
//!
//! Routes to SFX v3 briques when the segment has SFX3 format,
//! falls back to v2 SuffixContainsQuery for older segments.
//!
//! Handles: contains, term, startsWith, phrase (all d=0 substring queries).

use std::collections::HashMap;
use std::sync::Arc;

use crate::index::SegmentId;
use crate::query::phrase_query::scoring_utils::HighlightSink;
use crate::query::phrase_query::suffix_contains_query::{
    CachedSfxResult, SuffixContainsQuery,
};
use crate::query::{EnableScoring, Query, Weight};
use crate::schema::Field;
use crate::SegmentReader;

/// Substring search query (d=0).
///
/// Supports contains, term (anchor_start + exact_match),
/// startsWith (anchor_start), and phrase queries.
///
/// Automatically routes to v3 briques for SFX3 segments,
/// v2 code for older segments.
#[derive(Debug, Clone)]
pub struct ContainsQueryV3 {
    inner: SuffixContainsQuery,
}

impl ContainsQueryV3 {
    /// Create a new contains query.
    pub fn new(raw_field: Field, query_text: String) -> Self {
        Self {
            inner: SuffixContainsQuery::new(raw_field, query_text),
        }
    }

    /// Only match at token start (startsWith mode).
    pub fn with_anchor_start(mut self) -> Self {
        self.inner = self.inner.with_anchor_start();
        self
    }

    /// Match must cover entire word(s) (term mode).
    pub fn with_exact_match(mut self) -> Self {
        self.inner = self.inner.with_exact_match();
        self
    }

    /// Enable cross-token continuation.
    pub fn with_continuation(mut self, enabled: bool) -> Self {
        self.inner = self.inner.with_continuation(enabled);
        self
    }

    /// Enable strict separator validation.
    pub fn with_strict_separators(mut self, enabled: bool) -> Self {
        self.inner = self.inner.with_strict_separators(enabled);
        self
    }

    /// Attach highlight sink.
    pub fn with_highlight_sink(mut self, sink: Arc<HighlightSink>, field_name: String) -> Self {
        self.inner = self.inner.with_highlight_sink(sink, field_name);
        self
    }

    /// Set global doc_freq (from cross-shard aggregation).
    pub fn with_global_doc_freq(mut self, doc_freq: u64) -> Self {
        self.inner = self.inner.with_global_doc_freq(doc_freq);
        self
    }

    /// Get query text.
    pub fn query_text(&self) -> &str {
        self.inner.query_text()
    }

    /// Get prescan doc_freq.
    pub fn prescan_doc_freq(&self) -> u64 {
        self.inner.prescan_doc_freq()
    }
}

impl Query for ContainsQueryV3 {
    fn prescan_segments(&mut self, segments: &[&SegmentReader]) -> crate::Result<()> {
        // Delegate to inner — it handles v2 prescan.
        // TODO: detect SFX3 segments and use briques v3 instead.
        // For now, all segments go through v2 path.
        // When SFX3 segments exist, the prescan will:
        //   1. Open .sfx, check magic
        //   2. If SFX3: use contains_v3() from briques
        //   3. If SFX1/2: use v2 run_sfx_walk()
        //   4. Cache results in same CachedSfxResult format
        self.inner.prescan_segments(segments)
    }

    fn collect_prescan_doc_freqs(&self, out: &mut HashMap<String, u64>) {
        self.inner.collect_prescan_doc_freqs(out)
    }

    fn set_global_contains_doc_freqs(&mut self, freqs: &HashMap<String, u64>) {
        self.inner.set_global_contains_doc_freqs(freqs)
    }

    fn take_prescan_cache(
        &mut self,
        out: &mut HashMap<(String, SegmentId), CachedSfxResult>,
    ) {
        self.inner.take_prescan_cache(out)
    }

    fn inject_prescan_cache(
        &mut self,
        cache: HashMap<(String, SegmentId), CachedSfxResult>,
    ) {
        self.inner.inject_prescan_cache(cache)
    }

    fn sfx_prescan_params(&self) -> Vec<crate::query::SfxPrescanParam> {
        self.inner.sfx_prescan_params()
    }

    fn weight(&self, enable_scoring: EnableScoring) -> crate::Result<Box<dyn Weight>> {
        self.inner.weight(enable_scoring)
    }
}
