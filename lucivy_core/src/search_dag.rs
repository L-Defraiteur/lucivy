//! DAG-based search orchestration for sharded indexes.
//!
//! ```text
//! drain ── flush ──┬── prescan_0 ──┐                    ┌── search_0 ──┐
//!                  ├── prescan_1 ──┼── merge_prescan ── build_weight ──┼── search_1 ──┼── merge
//!                  └── prescan_2 ──┘                    └── search_2 ──┘
//! ```
//!
//! Prescan nodes run SFX walks (contains/startsWith) and regex walks in
//! parallel (one per shard) for globally consistent BM25 scoring.

use std::collections::{BinaryHeap, HashMap, HashSet};
use std::sync::Arc;

use luciole::node::{Node, NodeContext, PortDef};
use luciole::port::{PortType, PortValue};
use luciole::Dag;

use ld_lucivy::query::Weight;
use ld_lucivy::schema::Schema;

use crate::bm25_global::AggregatedBm25StatsOwned;
use crate::handle::LucivyHandle;
use crate::query::QueryConfig;
use crate::sharded_handle::{ShardMsg, ShardedSearchResult, ScoredEntry};

/// Prescan result from one shard:
/// (sfx_cache, sfx_freqs, regex_cache, regex_freqs).
type PrescanResult = (
    HashMap<(String, ld_lucivy::index::SegmentId), ld_lucivy::query::CachedSfxResult>,
    HashMap<String, u64>,
    HashMap<ld_lucivy::index::SegmentId, ld_lucivy::query::CachedRegexResult>,
    HashMap<String, u64>,
);

// ---------------------------------------------------------------------------
// DrainNode — flush ingestion pipeline
// ---------------------------------------------------------------------------

pub(crate) struct DrainNode {
    pipeline: Arc<luciole::StreamDag>,
}

impl DrainNode {
    pub fn new(pipeline: Arc<luciole::StreamDag>) -> Self {
        Self { pipeline }
    }
}

impl Node for DrainNode {
    fn node_type(&self) -> &'static str { "drain_pipeline" }
    fn outputs(&self) -> Vec<PortDef> {
        vec![PortDef::trigger("done")]
    }
    fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
        self.pipeline.drain();
        ctx.info("pipeline drained");
        ctx.trigger("done");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// FlushNode — commit uncommitted shards
// ---------------------------------------------------------------------------

pub(crate) struct FlushNode {
    shards: Vec<Arc<LucivyHandle>>,
    shard_pool: luciole::Pool<ShardMsg>,
}

impl FlushNode {
    pub fn new(shards: Vec<Arc<LucivyHandle>>, shard_pool: luciole::Pool<ShardMsg>) -> Self {
        Self { shards, shard_pool }
    }
}

impl Node for FlushNode {
    fn node_type(&self) -> &'static str { "flush_uncommitted" }
    fn inputs(&self) -> Vec<PortDef> {
        vec![PortDef::trigger("trigger")]
    }
    fn outputs(&self) -> Vec<PortDef> {
        vec![PortDef::trigger("done")]
    }
    fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
        let mut flushed = 0u32;
        for i in 0..self.shard_pool.len() {
            if self.shards[i].has_uncommitted() {
                let _ = self.shard_pool.worker(i).request(
                    |r| ShardMsg::Commit { fast: false, reply: r },
                    "flush_shard",
                );
                flushed += 1;
            }
        }
        ctx.metric("shards_flushed", flushed as f64);
        ctx.info(&format!("flushed {flushed} shards"));
        ctx.trigger("done");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// PrescanShardNode — SFX walk on ONE segment (one node per shard x segment)
// ---------------------------------------------------------------------------
//
// This used to be one node per shard, each looping over that shard's segments
// sequentially. Because the loop ran inside an actor handler, nothing underneath
// it could fan out again: execute_dag deliberately runs nested DAGs inline to
// avoid pool starvation, so a shard's segments were always walked one by one.
// Flattening the fan-out to (shard, segment) puts every unit of prescan work at
// the same DAG level, so the scheduler parallelises all of it at once and the two
// levers — sharding and per-segment parallelism — finally compose.

pub(crate) struct PrescanShardNode {
    segments: Vec<ld_lucivy::index::SegmentReader>,
    sfx_prescan_params: Vec<ld_lucivy::query::SfxPrescanParam>,
    regex_prescan_params: Vec<ld_lucivy::query::RegexPrescanParam>,
}

impl PrescanShardNode {
    pub fn new(
        segments: Vec<ld_lucivy::index::SegmentReader>,
        sfx_prescan_params: Vec<ld_lucivy::query::SfxPrescanParam>,
        regex_prescan_params: Vec<ld_lucivy::query::RegexPrescanParam>,
    ) -> Self {
        Self { segments, sfx_prescan_params, regex_prescan_params }
    }
}

impl Node for PrescanShardNode {
    fn node_type(&self) -> &'static str { "prescan_shard" }
    fn inputs(&self) -> Vec<PortDef> {
        vec![PortDef::trigger("trigger")]
    }
    fn outputs(&self) -> Vec<PortDef> {
        vec![PortDef::required("prescan", PortType::of::<PrescanResult>())]
    }
    fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
        if self.sfx_prescan_params.is_empty() && self.regex_prescan_params.is_empty() {
            ctx.set_output("prescan", PortValue::new(
                (HashMap::new(), HashMap::new(), HashMap::new(), HashMap::new()) as PrescanResult,
            ));
            return Ok(());
        }

        use ld_lucivy::query::{
            run_sfx_walk, tokenize_query, CachedSfxResult, CachedRegexResult,
            RawPostingEntry, build_resolver, run_regex_prescan, run_sfx_v3_prescan,
        };
        use ld_lucivy::suffix_fst::file::SfxFileReader;
        use ld_lucivy::suffix_fst::section_file::detect_sfx_version;

        let mut sfx_cache = HashMap::new();
        let mut sfx_freqs: HashMap<String, u64> = HashMap::new();
        let mut regex_cache = HashMap::new();
        let mut regex_freqs: HashMap<String, u64> = HashMap::new();

        // --- SFX prescan (existing logic, unchanged) ---
        for seg_reader in &self.segments {
            for param in &self.sfx_prescan_params {
                let sfx_data = match seg_reader.sfx_file(param.field) {
                    Some(d) => d,
                    None => continue,
                };
                let sfx_bytes = sfx_data.read_bytes()
                    .map_err(|e| format!("read sfx: {e}"))?;

                // Dispatch on the on-disk format, exactly like
                // ContainsQueryV3::prescan_segments does. This node used to open
                // every .sfx with the v1/v2 reader unconditionally, so any v3 index
                // reached through ShardedHandle failed outright on "invalid .sfx
                // magic bytes" — v3 + sharding had simply never been wired.
                let version = detect_sfx_version(sfx_bytes.as_ref()).unwrap_or(1);
                // v3 segments are not prescanned here: `BuildWeightNode` hands
                // every v3 segment of every shard to `Query::prescan_segments`
                // at once — the same call `search_with_global_stats` makes —
                // which is the only place that knows how to prescan contains,
                // fuzzy AND regex v3 (each is parallel inside). This node used
                // to run the contains walk for v3 and skip fuzzy, so fuzzy and
                // regex prescanned themselves in `weight()` on shard 0 only:
                // a 4-shard index answered a quarter of the fuzzy results
                // (distributed coherence panel, 23 August).
                if version == 3 {
                    continue;
                }
                let (doc_tf, highlights) = {
                    let sfx_reader = SfxFileReader::open(sfx_bytes.as_ref())
                        .map_err(|e| format!("open sfx: {e}"))?;

                    let pr = build_resolver(seg_reader, param.field)
                        .map_err(|e| format!("resolver: {e}"))?;
                    let resolver = |ord: u64| -> Vec<RawPostingEntry> {
                        pr.resolve(ord).into_iter().map(|e| {
                            RawPostingEntry {
                                doc_id: e.doc_id, token_index: e.position,
                                byte_from: e.byte_from, byte_to: e.byte_to,
                            }
                        }).collect()
                    };

                    let (tokens, seps) = tokenize_query(&param.query_text);
                    let seg_str = format!("{:?}", seg_reader.segment_id());
                    run_sfx_walk(
                        &sfx_reader, &resolver, &param.query_text,
                        &tokens, &seps,
                        param.anchor_start, param.exact_match,
                        param.continuation, param.strict_separators,
                        Some(&seg_str),
                        None,
                    )
                };

                let freq_key = format!("{}:{}", param.field.field_id(), param.query_text);
                *sfx_freqs.entry(freq_key.clone()).or_insert(0) += doc_tf.len() as u64;
                if !doc_tf.is_empty() {
                    sfx_cache.insert((freq_key, seg_reader.segment_id()), CachedSfxResult::new(doc_tf, highlights));
                }
            }
        }

        // --- Regex prescan (NEW) ---
        // DFA compiled inside run_regex_prescan per segment.
        for param in &self.regex_prescan_params {
            for seg_reader in &self.segments {
                // v3 segments: see above, prescanned by the query itself.
                let is_v3 = seg_reader.sfx_file(param.field)
                    .and_then(|d| d.read_bytes().ok())
                    .and_then(|b| detect_sfx_version(b.as_ref()))
                    == Some(3);
                if is_v3 { continue; }
                let (doc_tf, highlights) = run_regex_prescan(
                    seg_reader, param.field, &param.pattern, param.anchor_start,
                ).map_err(|e| format!("regex prescan: {e}"))?;

                *regex_freqs.entry(param.pattern.clone()).or_insert(0) += doc_tf.len() as u64;
                if !doc_tf.is_empty() {
                    regex_cache.insert(seg_reader.segment_id(), CachedRegexResult {
                        doc_tf, highlights, doc_coverage: Vec::new(),
                    });
                }
            }
        }

        ctx.metric("segments_scanned", (sfx_cache.len() + regex_cache.len()) as f64);
        ctx.set_output("prescan", PortValue::new(
            (sfx_cache, sfx_freqs, regex_cache, regex_freqs) as PrescanResult,
        ));
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// MergePrescanNode — aggregate prescan results from all shards
// ---------------------------------------------------------------------------

pub(crate) struct MergePrescanNode {
    /// Number of prescan nodes to gather — one per (shard, segment) pair.
    num_shards: usize,
}

impl MergePrescanNode {
    pub fn new(num_prescan_nodes: usize) -> Self {
        Self { num_shards: num_prescan_nodes }
    }
}

impl Node for MergePrescanNode {
    fn node_type(&self) -> &'static str { "merge_prescan" }
    fn inputs(&self) -> Vec<PortDef> {
        (0..self.num_shards)
            .map(|i| PortDef::required(
                Box::leak(format!("prescan_{i}").into_boxed_str()),
                PortType::of::<PrescanResult>(),
            ))
            .collect()
    }
    fn outputs(&self) -> Vec<PortDef> {
        vec![PortDef::required("merged", PortType::of::<PrescanResult>())]
    }
    fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
        let mut merged_sfx_cache = HashMap::new();
        let mut merged_sfx_freqs: HashMap<String, u64> = HashMap::new();
        let mut merged_regex_cache = HashMap::new();
        let mut merged_regex_freqs: HashMap<String, u64> = HashMap::new();

        for i in 0..self.num_shards {
            let port = format!("prescan_{i}");
            if let Some(value) = ctx.take_input(&port) {
                if let Some((sfx_cache, sfx_freqs, regex_cache, regex_freqs)) = value.take::<PrescanResult>() {
                    merged_sfx_cache.extend(sfx_cache);
                    for (key, freq) in sfx_freqs {
                        *merged_sfx_freqs.entry(key).or_insert(0) += freq;
                    }
                    merged_regex_cache.extend(regex_cache);
                    for (key, freq) in regex_freqs {
                        *merged_regex_freqs.entry(key).or_insert(0) += freq;
                    }
                }
            }
        }

        ctx.metric("total_segments", (merged_sfx_cache.len() + merged_regex_cache.len()) as f64);
        ctx.set_output("merged", PortValue::new(
            (merged_sfx_cache, merged_sfx_freqs, merged_regex_cache, merged_regex_freqs) as PrescanResult,
        ));
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// BuildWeightNode — compile Weight with prescan results + global stats
// ---------------------------------------------------------------------------

pub(crate) struct BuildWeightNode {
    shards: Vec<Arc<LucivyHandle>>,
    /// Pre-built query (constructed once before the DAG, not inside it).
    query: Box<dyn ld_lucivy::query::Query>,
}

impl BuildWeightNode {
    pub fn new(
        shards: Vec<Arc<LucivyHandle>>,
        query: Box<dyn ld_lucivy::query::Query>,
    ) -> Self {
        Self { shards, query }
    }
}

impl Node for BuildWeightNode {
    fn node_type(&self) -> &'static str { "build_weight" }
    fn inputs(&self) -> Vec<PortDef> {
        vec![
            PortDef::optional("prescan", PortType::of::<PrescanResult>()),
            PortDef::optional("trigger", PortType::Trigger),  // for fast path (no prescan)
        ]
    }
    fn outputs(&self) -> Vec<PortDef> {
        vec![PortDef::required("weight", PortType::of::<Arc<dyn Weight>>())]
    }
    fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
        let searchers: Vec<_> = self.shards.iter().map(|s| s.reader.searcher()).collect();
        let global_stats = AggregatedBm25StatsOwned::new(searchers);

        // Get prescan results from merge_prescan node
        let (sfx_cache, sfx_freqs, regex_cache, regex_freqs) = ctx.take_input("prescan")
            .and_then(|v| v.take::<PrescanResult>())
            .unwrap_or_default();

        // v3 segments of every shard, prescanned by the query itself (see
        // PrescanShardNode). Sets each sub-query's cache and its doc_freq
        // summed over all shards of this node.
        let v3_segments: Vec<ld_lucivy::SegmentReader> = self.shards.iter()
            .flat_map(|s| s.reader.searcher().segment_readers().to_vec())
            .filter(|seg| {
                let Some(field) = seg.sfx_fields().next() else { return false };
                seg.sfx_file(field)
                    .and_then(|d| d.read_bytes().ok())
                    .and_then(|b| ld_lucivy::suffix_fst::section_file::detect_sfx_version(b.as_ref()))
                    == Some(3)
            })
            .collect();
        let mut sfx_freqs = sfx_freqs;
        if !v3_segments.is_empty() {
            let refs: Vec<&ld_lucivy::SegmentReader> = v3_segments.iter().collect();
            self.query.prescan_segments(&refs).map_err(|e| format!("v3 prescan: {e}"))?;
            let mut v3_freqs = HashMap::new();
            self.query.collect_prescan_doc_freqs(&mut v3_freqs);
            for (k, v) in v3_freqs {
                *sfx_freqs.entry(k).or_insert(0) += v;
            }
        }

        // Inject SFX prescan results (v2 segments' cache extends the v3 one)
        if !sfx_freqs.is_empty() {
            self.query.set_global_contains_doc_freqs(&sfx_freqs);
            self.query.inject_prescan_cache(sfx_cache);
        }

        // Inject regex prescan results (v2 segments)
        if !regex_freqs.is_empty() {
            self.query.set_global_regex_doc_freqs(&regex_freqs);
            self.query.inject_regex_prescan_cache(regex_cache);
        }

        let searcher_0 = self.shards[0].reader.searcher();
        let enable_scoring = ld_lucivy::query::EnableScoring::enabled_from_statistics_provider(
            Arc::new(global_stats), &searcher_0,
        );
        let weight: Arc<dyn Weight> = self.query
            .weight(enable_scoring)
            .map_err(|e| format!("weight: {e}"))?
            .into();

        ctx.metric("compiled", 1.0);
        ctx.set_output("weight", PortValue::new(weight));
        Ok(())
    }
}

// extract_contains_terms removed — prescan params are now derived from the
// built query via sfx_prescan_params(), so there's a single source of truth.

// ---------------------------------------------------------------------------
// SearchShardNode — execute weight on one shard (via shard pool)
// ---------------------------------------------------------------------------

pub(crate) struct SearchShardNode {
    shard_pool: luciole::Pool<ShardMsg>,
    shard_id: usize,
    top_k: usize,
    filter: Option<Arc<HashSet<u64>>>,
}

impl SearchShardNode {
    pub fn new(shard_pool: luciole::Pool<ShardMsg>, shard_id: usize, top_k: usize) -> Self {
        Self { shard_pool, shard_id, top_k, filter: None }
    }

    pub fn with_filter(mut self, filter: Arc<HashSet<u64>>) -> Self {
        self.filter = Some(filter);
        self
    }
}

impl Node for SearchShardNode {
    fn node_type(&self) -> &'static str { "search_shard" }
    fn inputs(&self) -> Vec<PortDef> {
        vec![
            PortDef::required("weight", PortType::of::<Arc<dyn Weight>>()),
            PortDef::optional("trigger", PortType::Trigger),
        ]
    }
    fn outputs(&self) -> Vec<PortDef> {
        vec![PortDef::required("hits", PortType::of::<Vec<ShardedSearchResult>>())]
    }
    fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
        let weight = ctx.input("weight")
            .ok_or("missing weight")?
            .downcast::<Arc<dyn Weight>>()
            .ok_or("wrong weight type")?
            .clone();

        let filter = self.filter.clone();
        let result = self.shard_pool.worker(self.shard_id).request(
            |r| ShardMsg::Search { weight, top_k: self.top_k, filter, reply: r },
            "search_shard",
        ).map_err(|e| format!("shard_{} request: {e}", self.shard_id))?;

        let hits = result.map_err(|e| format!("shard_{}: {e}", self.shard_id))?;
        let results: Vec<ShardedSearchResult> = hits.into_iter()
            .map(|(score, addr)| ShardedSearchResult {
                score, shard_id: self.shard_id, doc_address: addr,
            })
            .collect();

        ctx.metric("hits", results.len() as f64);
        ctx.set_output("hits", PortValue::new(results));
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// MergeResultsNode — binary heap merge from all shards
// ---------------------------------------------------------------------------

pub(crate) struct MergeResultsNode {
    num_shards: usize,
    top_k: usize,
}

impl MergeResultsNode {
    pub fn new(num_shards: usize, top_k: usize) -> Self {
        Self { num_shards, top_k }
    }
}

impl Node for MergeResultsNode {
    fn node_type(&self) -> &'static str { "merge_results" }
    fn inputs(&self) -> Vec<PortDef> {
        (0..self.num_shards)
            .map(|i| PortDef::required(
                Box::leak(format!("hits_{i}").into_boxed_str()),
                PortType::of::<Vec<ShardedSearchResult>>(),
            ))
            .collect()
    }
    fn outputs(&self) -> Vec<PortDef> {
        vec![PortDef::required("results", PortType::of::<Vec<ShardedSearchResult>>())]
    }
    fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
        let mut heap = BinaryHeap::with_capacity(self.top_k + 1);

        for i in 0..self.num_shards {
            let port = format!("hits_{i}");
            if let Some(value) = ctx.take_input(&port) {
                if let Some(hits) = value.take::<Vec<ShardedSearchResult>>() {
                    for r in hits {
                        heap.push(ScoredEntry {
                            score: r.score, shard_id: r.shard_id, doc_address: r.doc_address,
                        });
                        if heap.len() > self.top_k { heap.pop(); }
                    }
                }
            }
        }

        // into_sorted_vec() returns ascending by Ord. Our Ord is reversed
        // (min-heap), so ascending = highest score first. No reverse needed.
        let results: Vec<ShardedSearchResult> = heap
            .into_sorted_vec()
            .into_iter()
            .map(|e| ShardedSearchResult {
                score: e.score, shard_id: e.shard_id, doc_address: e.doc_address,
            })
            .collect();

        ctx.metric("total_results", results.len() as f64);
        ctx.set_output("results", PortValue::new(results));
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// OutputNode — convergence point for single/multi shard results
// ---------------------------------------------------------------------------

pub(crate) struct OutputNode;

impl Node for OutputNode {
    fn node_type(&self) -> &'static str { "output" }
    fn inputs(&self) -> Vec<PortDef> {
        vec![PortDef::required("results", PortType::of::<Vec<ShardedSearchResult>>())]
    }
    fn outputs(&self) -> Vec<PortDef> {
        vec![PortDef::required("results", PortType::of::<Vec<ShardedSearchResult>>())]
    }
    fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
        if let Some(value) = ctx.take_input("results") {
            ctx.set_output("results", value);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// build_search_dag — factory
// ---------------------------------------------------------------------------

pub(crate) fn build_search_dag(
    shards: &[Arc<LucivyHandle>],
    shard_pool: &luciole::Pool<ShardMsg>,
    pipeline: &Arc<luciole::StreamDag>,
    schema: &Schema,
    query_config: &QueryConfig,
    top_k: usize,
    highlight_sink: Option<Arc<ld_lucivy::query::HighlightSink>>,
    filter: Option<Arc<HashSet<u64>>>,
) -> Result<Dag, String> {
    let mut dag = Dag::new();
    let num_shards = shards.len();
    let is_multi = num_shards > 1;

    // Build the query once BEFORE the DAG — avoids DFA/regex compilation inside the DAG.
    let query = crate::query::build_query(
        query_config, schema, &shards[0].index, highlight_sink,
    )?;
    let sfx_prescan_params = query.sfx_prescan_params();
    let regex_prescan_params = query.regex_prescan_params();

    let needs_prescan = !sfx_prescan_params.is_empty() || !regex_prescan_params.is_empty();

    // drain → flush
    dag.add_node("drain", DrainNode::new(Arc::clone(pipeline)));
    dag.add_node("flush", FlushNode::new(shards.to_vec(), shard_pool.clone()));
    dag.connect("drain", "done", "flush", "trigger")?;

    // flush → branch: needs prescan?
    dag.add_node("needs_prescan", luciole::BranchNode(move || needs_prescan));
    dag.connect("flush", "done", "needs_prescan", "trigger")?;

    // ── Prescan path ───────────────────────────────────────────────────
    //
    // N>1: prescan_0..N ∥ → merge_prescan → build_weight
    // N=1: prescan_0 ──────────────────────→ build_weight  (no merge)

    // One prescan node per (shard, segment), not per shard. Flat fan-out means the
    // scheduler runs every segment concurrently; grouping by shard would force each
    // shard's segments to be walked sequentially inside an actor handler.
    let mut prescan_units: Vec<Vec<ld_lucivy::index::SegmentReader>> = Vec::new();
    for shard in shards.iter() {
        let searcher = shard.reader.searcher();
        for seg in searcher.segment_readers() {
            prescan_units.push(vec![seg.clone()]);
        }
    }
    // No segment anywhere (empty index): keep one node so the wiring below holds.
    if prescan_units.is_empty() {
        prescan_units.push(Vec::new());
    }
    let num_prescan = prescan_units.len();

    for (i, segments) in prescan_units.into_iter().enumerate() {
        let node_name = format!("prescan_{i}");
        dag.add_node(&node_name, PrescanShardNode::new(
            segments,
            sfx_prescan_params.clone(),
            regex_prescan_params.clone(),
        ));
        dag.connect("needs_prescan", "then", &node_name, "trigger")?;
    }

    dag.add_node("build_weight", BuildWeightNode::new(shards.to_vec(), query));
    dag.connect("needs_prescan", "else", "build_weight", "trigger")?;

    if num_prescan > 1 {
        dag.add_node("merge_prescan", MergePrescanNode::new(num_prescan));
        for i in 0..num_prescan {
            dag.connect(
                &format!("prescan_{i}"), "prescan",
                "merge_prescan", &format!("prescan_{i}"),
            )?;
        }
        dag.connect("merge_prescan", "merged", "build_weight", "prescan")?;
    } else {
        dag.connect("prescan_0", "prescan", "build_weight", "prescan")?;
    }

    // ── Search path ────────────────────────────────────────────────────
    //
    // N>1: search_0..N ∥ → merge_results → output
    // N=1: search_0 ─────────────────────→ output  (no merge)

    for i in 0..num_shards {
        let mut node = SearchShardNode::new(shard_pool.clone(), i, top_k);
        if let Some(ref f) = filter {
            node = node.with_filter(Arc::clone(f));
        }
        dag.add_node(&format!("search_{i}"), node);
        dag.connect("build_weight", "weight", &format!("search_{i}"), "weight")?;
    }

    dag.add_node("output", OutputNode);

    if is_multi {
        dag.add_node("merge", MergeResultsNode::new(num_shards, top_k));
        for i in 0..num_shards {
            dag.connect(
                &format!("search_{i}"), "hits",
                "merge", &format!("hits_{i}"),
            )?;
        }
        dag.connect("merge", "results", "output", "results")?;
    } else {
        dag.connect("search_0", "hits", "output", "results")?;
    }

    Ok(dag)
}
