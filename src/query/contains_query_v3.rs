//! ContainsQueryV3 — standalone substring search (v2 + v3 segments).
//!
//! Owns its prescan cache and creates SfxWeight directly.
//! No wrapper around SuffixContainsQuery — this IS the primary query type.
//!
//! Handles: contains, term, startsWith, phrase (all d=0 substring queries).

use std::collections::HashMap;
use std::sync::Arc;

use crate::index::SegmentId;
use crate::query::phrase_query::scoring_utils::HighlightSink;
use crate::query::phrase_query::sfx_scoring::{CachedPrescan, SfxWeight, count_tf_sorted};
use crate::query::{EnableScoring, Query, Weight};
use crate::schema::Field;
use crate::{DocId, SegmentReader};

// Re-export for backward compat (sharded_handle, search_dag, etc.)
pub use crate::query::phrase_query::sfx_scoring::CachedPrescan as CachedSfxResult;

/// Substring search query (d=0).
///
/// Supports contains, term (anchor_start + exact_match),
/// startsWith (anchor_start), and phrase queries.
///
/// Automatically routes to v3 briques for SFX3 segments,
/// v2 code for older segments.
#[derive(Debug, Clone)]
pub struct ContainsQueryV3 {
    field: Field,
    query_text: String,
    anchor_start: bool,
    exact_match: bool,
    strict_separators: bool,
    highlight_sink: Option<Arc<HighlightSink>>,
    highlight_field_name: String,
    prescan_cache: HashMap<(String, SegmentId), CachedPrescan>,
    global_doc_freq: u64,
}

/// Prescan one segment with the SFX v3 pipeline.
///
/// Free function on purpose: the sharded search DAG needs the exact same walk as
/// ContainsQueryV3, and used to hand-roll its own — with the v1/v2 reader, which
/// simply fails on an SFX3 file. `Query::sfx_prescan_params` exists so the DAG can
/// run "the exact same parameters as the query itself, no duplication, no
/// mismatch"; this is the other half of that promise.
pub fn run_sfx_v3_prescan(
seg_reader: &SegmentReader,
sfx_bytes: &[u8],
field: crate::schema::Field,
query_text: &str,
anchor_start: bool,
exact_match: bool,
strict_separators: bool,
) -> crate::Result<(Vec<(DocId, u32)>, Vec<(DocId, usize, usize)>)> {
    use crate::suffix_fst::file_v3::SfxFileReaderV3;
    use crate::suffix_fst::briques::{orchestrator, context::BriquesContext};

    let reader = SfxFileReaderV3::open(sfx_bytes).map_err(|e|
        crate::LucivyError::SystemError(format!("open SFX3: {e}")))?;
    let pr = crate::query::posting_resolver::build_resolver(seg_reader, field)?;

    let load = |ext: &str| -> Option<Vec<u8>> {
        seg_reader.sfx_index_file(ext, field)
            .and_then(|fs| fs.read_bytes().ok())
            .map(|b| b.as_ref().to_vec())
    };
    let posmap_bytes = load("posmap");
    let bytemap_bytes = load("bytemap");
    let wsp_bytes = load("word_sfxpost");
    let sib_bytes = load("sibling_v3");
    let tt_bytes = load("termtexts");

    let debug_query = std::env::var("V3_DEBUG_QUERY").ok();
    let do_debug = debug_query.as_deref() == Some(query_text);
    let trace_id = if do_debug {
        Some(crate::suffix_fst::briques::trace::trace_begin())
    } else {
        None
    };

    let ctx = BriquesContext {
        reader: &reader,
        resolver: &*pr,
        filter_docs: None,
        debug: do_debug,
        trace_id,
        posmap: posmap_bytes.as_ref().and_then(|b| crate::suffix_fst::posmap::PosMapReader::open(b)),
        bytemap: bytemap_bytes.as_ref().and_then(|b| crate::suffix_fst::bytemap::ByteBitmapReader::open(b)),
        word_sfxpost: wsp_bytes.as_ref().and_then(|b| crate::suffix_fst::word_sfxpost::WordSfxPostReader::open(b)),
        sibling_v3: sib_bytes.as_ref().and_then(|b| crate::suffix_fst::sibling_table::SiblingTableReader::open(b)),
        termtexts: tt_bytes.as_ref().and_then(|b| crate::suffix_fst::termtexts_v3::TermTextsReaderV3::open(b)),
    };

    let mut matches = orchestrator::contains_v3(
        &ctx, query_text,
        anchor_start, exact_match, strict_separators,
    );

    // A word_pos_map post-filter used to sit here. Its retain closure returned
    // `true` from both branches — intra-word and inter-word alike — so it never
    // rejected anything, while loading word_pos_map, next_word_map and
    // chunk_word_map on every segment scan. It read as a safety net and was not
    // one. Removed rather than left in place; if inter-word verification is
    // wanted, it has to be written, not resurrected.

    let highlights: Vec<(DocId, usize, usize)> = matches.iter()
        .map(|m| (m.doc_id, m.byte_from as usize, m.byte_to as usize))
        .collect();
    let mut doc_ids: Vec<DocId> = matches.iter().map(|m| m.doc_id).collect();
    doc_ids.sort_unstable();
    Ok((count_tf_sorted(&doc_ids), highlights))
}


impl ContainsQueryV3 {
    /// Prescan a single segment. `None` when the segment has no SFX file.
    fn prescan_one(
        &self,
        seg_reader: &SegmentReader,
    ) -> crate::Result<Option<(crate::index::SegmentId, Vec<(DocId, u32)>, Vec<(DocId, usize, usize)>)>> {
        use crate::suffix_fst::section_file::detect_sfx_version;

        let segment_id = seg_reader.segment_id();
        let sfx_data = match seg_reader.sfx_file(self.field) {
            Some(d) => d,
            None => return Ok(None),
        };
        let sfx_bytes = sfx_data.read_bytes().map_err(|e|
            crate::LucivyError::SystemError(format!("prescan read .sfx: {e}")))?;

        let version = detect_sfx_version(sfx_bytes.as_ref()).unwrap_or(1);
        let (doc_tf, highlights) = if version == 3 {
            self.prescan_segment_v3(seg_reader, &sfx_bytes)?
        } else {
            self.prescan_segment_v2(seg_reader, &sfx_bytes)?
        };
        Ok(Some((segment_id, doc_tf, highlights)))
    }

    fn record_prescan(
        &mut self,
        segment_id: crate::index::SegmentId,
        doc_tf: Vec<(DocId, u32)>,
        highlights: Vec<(DocId, usize, usize)>,
    ) {
        self.global_doc_freq += doc_tf.len() as u64;
        self.prescan_cache.insert(
            (self.cache_key(), segment_id),
            CachedPrescan::new(doc_tf, highlights),
        );
    }

    pub fn new(raw_field: Field, query_text: String) -> Self {
        Self {
            field: raw_field,
            query_text,
            anchor_start: false,
            exact_match: false,
            strict_separators: false,
            highlight_sink: None,
            highlight_field_name: String::new(),
            prescan_cache: HashMap::new(),
            global_doc_freq: 0,
        }
    }

    pub fn with_anchor_start(mut self) -> Self { self.anchor_start = true; self }
    pub fn with_exact_match(mut self) -> Self { self.exact_match = true; self }
    pub fn with_continuation(self, _enabled: bool) -> Self { self } // v3 always does cross-token
    pub fn with_strict_separators(mut self, enabled: bool) -> Self { self.strict_separators = enabled; self }
    pub fn with_highlight_sink(mut self, sink: Arc<HighlightSink>, field_name: String) -> Self {
        self.highlight_sink = Some(sink);
        self.highlight_field_name = field_name;
        self
    }
    pub fn with_global_doc_freq(mut self, doc_freq: u64) -> Self { self.global_doc_freq = doc_freq; self }

    pub fn query_text(&self) -> &str { &self.query_text }
    pub fn prescan_doc_freq(&self) -> u64 { self.global_doc_freq }

    /// Cache key: "field_id:query_text" — consistent across prescan, weight, scorer.
    fn cache_key(&self) -> String {
        format!("{}:{}", self.field.field_id(), self.query_text)
    }

    // ─── Prescan per segment ──────────────────────────────────────────

    fn prescan_segment_v3(
        &self,
        seg_reader: &SegmentReader,
        sfx_bytes: &[u8],
    ) -> crate::Result<(Vec<(DocId, u32)>, Vec<(DocId, usize, usize)>)> {
        run_sfx_v3_prescan(
            seg_reader, sfx_bytes, self.field, &self.query_text,
            self.anchor_start, self.exact_match, self.strict_separators,
        )
    }

    fn prescan_segment_v2(
        &self,
        seg_reader: &SegmentReader,
        sfx_bytes: &[u8],
    ) -> crate::Result<(Vec<(DocId, u32)>, Vec<(DocId, usize, usize)>)> {
        use crate::suffix_fst::file::SfxFileReader;
        use crate::query::phrase_query::suffix_contains;
        use crate::query::phrase_query::suffix_contains_query::{run_sfx_walk, tokenize_query};

        let sfx_reader = SfxFileReader::open(sfx_bytes).map_err(|e|
            crate::LucivyError::SystemError(format!("open SFX v2: {e}")))?;
        let pr = crate::query::posting_resolver::build_resolver(seg_reader, self.field)?;
        let resolver = |raw_ordinal: u64| -> Vec<suffix_contains::RawPostingEntry> {
            pr.resolve(raw_ordinal).into_iter().map(|e| suffix_contains::RawPostingEntry {
                doc_id: e.doc_id, token_index: e.position,
                byte_from: e.byte_from, byte_to: e.byte_to,
            }).collect()
        };

        let termtexts_bytes = seg_reader.sfx_index_file("termtexts", self.field)
            .and_then(|fs| fs.read_bytes().ok())
            .map(|b| b.as_ref().to_vec());
        let termtexts_reader = termtexts_bytes.as_ref()
            .and_then(|b| crate::suffix_fst::termtexts::TermTextsReader::open(b));
        let ord_to_term_fn = |ord: u64| -> Option<String> {
            termtexts_reader.as_ref()?.text(ord as u32).map(|s| s.to_string())
        };

        let (query_tokens, query_separators) = tokenize_query(&self.query_text);
        let (doc_tf, highlights) = run_sfx_walk(
            &sfx_reader, &resolver, &self.query_text,
            &query_tokens, &query_separators,
            self.anchor_start, self.exact_match,
            false, self.strict_separators,
            None, Some(&ord_to_term_fn),
        );

        Ok((doc_tf, highlights))
    }

    // ─── Weight creation ──────────────────────────────────────────────

    fn make_weight(&self, enable_scoring: EnableScoring) -> crate::Result<Box<dyn Weight>> {
        // Read the global statistics provider, never the local searcher.
        //
        // Summing max_doc() over the searcher's own segments gives THIS shard's doc
        // count, while global_doc_freq is aggregated across ALL shards by the search
        // DAG. Mixing the two makes doc_freq exceed doc_count and trips the
        // assertion in bm25::idf (observed: "1291 >= 1400" on a 4-shard v3 index).
        // `stats` is exactly the abstraction for this — "same stats whether local
        // multi-shard or distributed" — and the v2 path has always used it.
        let (scoring_enabled, global_num_docs, global_num_tokens) = match enable_scoring {
            EnableScoring::Enabled { stats, .. } => {
                let nd = stats.total_num_docs().unwrap_or(0).max(1);
                let nt = stats.total_num_tokens(self.field).unwrap_or(0);
                (true, nd, nt)
            }
            _ => (false, 0, 0),
        };

        Ok(Box::new(SfxWeight {
            raw_field: self.field,
            cache_key: self.cache_key(),
            prescan_cache: self.prescan_cache.clone(),
            global_doc_freq: self.global_doc_freq,
            scoring_enabled,
            global_num_docs,
            global_num_tokens,
            highlight_sink: self.highlight_sink.clone(),
            highlight_field_name: self.highlight_field_name.clone(),
        }))
    }
}

// ─── Query trait ──────────────────────────────────────────────────────────

impl Query for ContainsQueryV3 {
    /// Prescan every segment, in parallel when the caller allows it.
    ///
    /// This is where a contains query spends essentially all its time: weight() is
    /// called before executor.map in Searcher::search_with_executor, so a
    /// multi-thread search executor parallelises a phase that costs nothing while
    /// this loop stays serial (measured: 1.0x speedup on 80 segments / 24 threads).
    ///
    /// Fan-out goes through luciole, never through raw threads: the scheduler is the
    /// only construct that survives the WASM build. execute_dag additionally runs
    /// every node inline when it detects a scheduler thread, an actor handler, or a
    /// cooperative wait, so the sharded path — where this already runs inside an
    /// actor — degrades to the sequential loop instead of nesting pools. Nothing to
    /// lose there: the shards are parallel with each other already.
    fn prescan_segments(&mut self, segments: &[&SegmentReader]) -> crate::Result<()> {
        self.prescan_cache.clear();
        self.global_doc_freq = 0;

        // One segment, or nothing to do: not worth building a DAG.
        if segments.len() <= 1 {
            for seg_reader in segments {
                if let Some((segment_id, doc_tf, highlights)) = self.prescan_one(seg_reader)? {
                    self.record_prescan(segment_id, doc_tf, highlights);
                }
            }
            return Ok(());
        }

        type SegOutcome = Option<(crate::index::SegmentId, Vec<(DocId, u32)>, Vec<(DocId, usize, usize)>)>;

        let names: Vec<String> = (0..segments.len()).map(|i| format!("seg_{i}")).collect();
        let mut tasks: Vec<(&str, Box<dyn FnOnce() -> Result<luciole::port::PortValue, String> + Send + 'static>)> =
            Vec::with_capacity(segments.len());

        for (i, seg_reader) in segments.iter().enumerate() {
            // SegmentReader is Arc-backed, so this clone is cheap and gives the
            // closure the 'static ownership the scheduler requires.
            let seg = (*seg_reader).clone();
            let probe = self.clone();
            tasks.push((names[i].as_str(), Box::new(move || {
                let outcome: SegOutcome = probe.prescan_one(&seg)
                    .map_err(|e| format!("prescan segment: {e}"))?;
                Ok(luciole::port::PortValue::new(outcome))
            })));
        }

        let mut dag = luciole::scatter::build_scatter_dag(tasks);
        let mut result = luciole::execute_dag(&mut dag, None)
            .map_err(|e| crate::LucivyError::SystemError(format!("prescan DAG: {e}")))?;
        let map = result
            .take_output::<std::collections::HashMap<String, luciole::port::PortValue>>("collect", "results")
            .ok_or_else(|| crate::LucivyError::SystemError("prescan DAG: no results".into()))?;
        let mut scatter = luciole::ScatterResults::from(map);

        // Drain in segment order so the cache is built deterministically.
        for name in &names {
            if let Some(Some((segment_id, doc_tf, highlights))) = scatter.take::<SegOutcome>(name) {
                self.record_prescan(segment_id, doc_tf, highlights);
            }
        }
        Ok(())
    }

    fn weight(&self, enable_scoring: EnableScoring) -> crate::Result<Box<dyn Weight>> {
        if self.prescan_cache.is_empty() {
            if let Some(searcher) = enable_scoring.searcher() {
                let mut clone = self.clone();
                let seg_refs: Vec<&SegmentReader> = searcher.segment_readers().iter().collect();
                clone.prescan_segments(&seg_refs)?;
                return clone.make_weight(enable_scoring);
            }
        }
        self.make_weight(enable_scoring)
    }

    fn collect_prescan_doc_freqs(&self, out: &mut HashMap<String, u64>) {
        out.insert(self.cache_key(), self.global_doc_freq);
    }

    fn set_global_contains_doc_freqs(&mut self, freqs: &HashMap<String, u64>) {
        if let Some(&freq) = freqs.get(&self.cache_key()) {
            self.global_doc_freq = freq;
        }
    }

    fn take_prescan_cache(
        &mut self,
        out: &mut HashMap<(String, SegmentId), CachedPrescan>,
    ) {
        out.extend(self.prescan_cache.drain());
    }

    fn inject_prescan_cache(
        &mut self,
        cache: HashMap<(String, SegmentId), CachedPrescan>,
    ) {
        // Only keep entries matching our cache_key
        let key = self.cache_key();
        for ((k, sid), v) in cache {
            if k == key {
                self.prescan_cache.insert((k, sid), v);
            }
        }
    }

    fn sfx_prescan_params(&self) -> Vec<crate::query::SfxPrescanParam> {
        vec![crate::query::SfxPrescanParam {
            field: self.field,
            query_text: self.query_text.clone(),
            anchor_start: self.anchor_start,
            fuzzy_distance: 0,
            continuation: false,
            exact_match: self.exact_match,
            strict_separators: self.strict_separators,
        }]
    }
}
