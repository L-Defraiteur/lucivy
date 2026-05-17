//! RegexQueryV3 — regex substring search via literal extraction + DFA.
//!
//! Routes to SFX v3 briques when the segment has SFX3 format,
//! falls back to v2 RegexContinuationQuery for older segments.

use std::collections::HashMap;
use std::sync::Arc;

use crate::index::SegmentId;
use crate::query::phrase_query::scoring_utils::HighlightSink;
use crate::query::phrase_query::suffix_contains_query::CachedSfxResult;
use crate::query::phrase_query::regex_continuation_query::RegexContinuationQuery;
use crate::query::{EnableScoring, Query, Weight};
use crate::schema::Field;
use crate::SegmentReader;

/// Regex substring search query.
///
/// Pipeline: literal extraction → resolve via briques → gap validation (DFA).
/// strict_separators = true always (the regex defines what matches).
///
/// Routes to v3 briques for SFX3 segments, v2 for older.
#[derive(Debug, Clone)]
pub struct RegexQueryV3 {
    inner: RegexContinuationQuery,
}

impl RegexQueryV3 {
    /// Create a new regex query.
    pub fn new(raw_field: Field, pattern: String, anchor_start: bool) -> Self {
        let inner = RegexContinuationQuery::from_regex(raw_field, pattern, anchor_start);
        Self { inner }
    }

    /// Attach highlight sink.
    pub fn with_highlight_sink(mut self, sink: Arc<HighlightSink>, field_name: String) -> Self {
        self.inner = self.inner.with_highlight_sink(sink, field_name);
        self
    }
}

impl Query for RegexQueryV3 {
    fn prescan_segments(&mut self, segments: &[&SegmentReader]) -> crate::Result<()> {
        // TODO: detect SFX3 → use regex_v3() from briques
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
