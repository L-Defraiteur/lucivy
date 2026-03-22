//! DAG-based search orchestration for sharded indexes.
//!
//! ```text
//! drain ── flush ──┬── prescan_0 ──┐                    ┌── search_0 ──┐
//!                  ├── prescan_1 ──┼── merge_prescan ── build_weight ──┼── search_1 ──┼── merge
//!                  └── prescan_2 ──┘                    └── search_2 ──┘
//! ```
//!
//! Prescan nodes run SFX walks in parallel (one per shard) for globally
//! consistent BM25 contains/startsWith scoring. For non-SFX query types
//! (term, phrase, regex, fuzzy) the prescan nodes are no-ops.

use std::collections::{BinaryHeap, HashMap};
use std::sync::Arc;

use luciole::node::{Node, NodeContext, PortDef};
use luciole::port::{PortType, PortValue};
use luciole::Dag;

use ld_lucivy::query::Weight;
use ld_lucivy::schema::Schema;
use ld_lucivy::{DocAddress, Index};

use crate::bm25_global::AggregatedBm25StatsOwned;
use crate::handle::LucivyHandle;
use crate::query::QueryConfig;
use crate::sharded_handle::{ShardMsg, ShardedSearchResult, ScoredEntry};

/// Prescan result from one shard: (segment_id → cached SFX results, query_text → doc_freq).
type PrescanResult = (
    HashMap<ld_lucivy::index::SegmentId, ld_lucivy::query::CachedSfxResult>,
    HashMap<String, u64>,
);

// ---------------------------------------------------------------------------
// DrainNode — flush ingestion pipeline
// ---------------------------------------------------------------------------

pub(crate) struct DrainNode {
    reader_pool: luciole::Pool<super::sharded_handle::ReaderMsg>,
    router_ref: luciole::ActorRef<super::sharded_handle::RouterMsg>,
}

impl DrainNode {
    pub fn new(
        reader_pool: luciole::Pool<super::sharded_handle::ReaderMsg>,
        router_ref: luciole::ActorRef<super::sharded_handle::RouterMsg>,
    ) -> Self {
        Self { reader_pool, router_ref }
    }
}

impl Node for DrainNode {
    fn node_type(&self) -> &'static str { "drain_pipeline" }
    fn outputs(&self) -> Vec<PortDef> {
        vec![PortDef::trigger("done")]
    }
    fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
        self.reader_pool.drain("search_drain_readers");
        self.router_ref.request(
            |r| super::sharded_handle::RouterMsg::Drain(luciole::DrainMsg(r)),
            "search_drain_router",
        ).ok();
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
        ctx.info(&format!("flushed {} shards", flushed));
        ctx.trigger("done");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// PrescanShardNode — SFX walk on one shard's segments (parallel per shard)
// ---------------------------------------------------------------------------

pub(crate) struct PrescanShardNode {
    shard: Arc<LucivyHandle>,
    prescan_params: Vec<ld_lucivy::query::SfxPrescanParam>,
}

impl PrescanShardNode {
    pub fn new(
        shard: Arc<LucivyHandle>,
        prescan_params: Vec<ld_lucivy::query::SfxPrescanParam>,
    ) -> Self {
        Self { shard, prescan_params }
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
        if self.prescan_params.is_empty() {
            ctx.set_output("prescan", PortValue::new(
                (HashMap::new(), HashMap::new()) as PrescanResult,
            ));
            return Ok(());
        }

        use ld_lucivy::query::{
            run_sfx_walk, tokenize_query, CachedSfxResult, RawPostingEntry,
            build_resolver,
        };
        use ld_lucivy::suffix_fst::file::SfxFileReader;

        let searcher = self.shard.reader.searcher();
        let mut cache = HashMap::new();
        let mut freqs: HashMap<String, u64> = HashMap::new();

        for seg_reader in searcher.segment_readers() {
            for param in &self.prescan_params {
                let sfx_data = match seg_reader.sfx_file(param.field) {
                    Some(d) => d,
                    None => continue,
                };
                let sfx_bytes = sfx_data.read_bytes()
                    .map_err(|e| format!("read sfx: {e}"))?;
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
                let (doc_tf, highlights) = run_sfx_walk(
                    &sfx_reader, &resolver, &param.query_text,
                    &tokens, &seps,
                    param.fuzzy_distance, param.prefix_only, param.continuation,
                );

                *freqs.entry(param.query_text.clone()).or_insert(0) += doc_tf.len() as u64;
                if !doc_tf.is_empty() {
                    cache.insert(seg_reader.segment_id(), CachedSfxResult::new(doc_tf, highlights));
                }
            }
        }

        ctx.metric("segments_scanned", cache.len() as f64);
        ctx.set_output("prescan", PortValue::new((cache, freqs) as PrescanResult));
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// MergePrescanNode — aggregate prescan results from all shards
// ---------------------------------------------------------------------------

pub(crate) struct MergePrescanNode {
    num_shards: usize,
}

impl MergePrescanNode {
    pub fn new(num_shards: usize) -> Self {
        Self { num_shards }
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
        let mut merged_cache = HashMap::new();
        let mut merged_freqs: HashMap<String, u64> = HashMap::new();

        for i in 0..self.num_shards {
            let port = format!("prescan_{i}");
            if let Some(value) = ctx.take_input(&port) {
                if let Some((cache, freqs)) = value.take::<PrescanResult>() {
                    merged_cache.extend(cache);
                    for (key, freq) in freqs {
                        *merged_freqs.entry(key).or_insert(0) += freq;
                    }
                }
            }
        }

        ctx.metric("total_segments", merged_cache.len() as f64);
        ctx.set_output("merged", PortValue::new((merged_cache, merged_freqs) as PrescanResult));
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// BuildWeightNode — compile Weight with prescan results + global stats
// ---------------------------------------------------------------------------

pub(crate) struct BuildWeightNode {
    shards: Vec<Arc<LucivyHandle>>,
    schema: Schema,
    index: Index,
    query_config: QueryConfig,
    highlight_sink: Option<Arc<ld_lucivy::query::HighlightSink>>,
}

impl BuildWeightNode {
    pub fn new(
        shards: Vec<Arc<LucivyHandle>>,
        schema: Schema,
        index: Index,
        query_config: QueryConfig,
        highlight_sink: Option<Arc<ld_lucivy::query::HighlightSink>>,
    ) -> Self {
        Self { shards, schema, index, query_config, highlight_sink }
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
        let (merged_cache, merged_freqs) = ctx.take_input("prescan")
            .and_then(|v| v.take::<PrescanResult>())
            .unwrap_or_default();

        // Build query once (with highlights)
        let mut query = crate::query::build_query(
            &self.query_config, &self.schema, &self.index,
            self.highlight_sink.clone(),
        )?;

        // Inject prescan results into the query
        if !merged_freqs.is_empty() {
            query.set_global_contains_doc_freqs(&merged_freqs);
            query.inject_prescan_cache(merged_cache);
        }

        let searcher_0 = self.shards[0].reader.searcher();
        let enable_scoring = ld_lucivy::query::EnableScoring::enabled_from_statistics_provider(
            &global_stats, &searcher_0,
        );
        let weight: Arc<dyn Weight> = query
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
}

impl SearchShardNode {
    pub fn new(shard_pool: luciole::Pool<ShardMsg>, shard_id: usize, top_k: usize) -> Self {
        Self { shard_pool, shard_id, top_k }
    }
}

impl Node for SearchShardNode {
    fn node_type(&self) -> &'static str { "search_shard" }
    fn inputs(&self) -> Vec<PortDef> {
        vec![PortDef::required("weight", PortType::of::<Arc<dyn Weight>>())]
    }
    fn outputs(&self) -> Vec<PortDef> {
        vec![PortDef::required("hits", PortType::of::<Vec<(usize, f32, DocAddress)>>())]
    }
    fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
        let weight = ctx.input("weight")
            .ok_or("missing weight")?
            .downcast::<Arc<dyn Weight>>()
            .ok_or("wrong weight type")?
            .clone();

        let result = self.shard_pool.worker(self.shard_id).request(
            |r| ShardMsg::Search { weight, top_k: self.top_k, reply: r },
            "search_shard",
        ).map_err(|e| format!("shard_{} request: {e}", self.shard_id))?;

        let hits = result.map_err(|e| format!("shard_{}: {e}", self.shard_id))?;
        let tagged: Vec<(usize, f32, DocAddress)> = hits.into_iter()
            .map(|(score, addr)| (self.shard_id, score, addr))
            .collect();

        ctx.metric("hits", tagged.len() as f64);
        ctx.set_output("hits", PortValue::new(tagged));
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
                Box::leak(format!("hits_{}", i).into_boxed_str()),
                PortType::of::<Vec<(usize, f32, DocAddress)>>(),
            ))
            .collect()
    }
    fn outputs(&self) -> Vec<PortDef> {
        vec![PortDef::required("results", PortType::of::<Vec<ShardedSearchResult>>())]
    }
    fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
        let mut heap = BinaryHeap::with_capacity(self.top_k + 1);

        for i in 0..self.num_shards {
            let port = format!("hits_{}", i);
            if let Some(value) = ctx.take_input(&port) {
                if let Some(hits) = value.take::<Vec<(usize, f32, DocAddress)>>() {
                    for (shard_id, score, doc_addr) in hits {
                        heap.push(ScoredEntry { score, shard_id, doc_address: doc_addr });
                        if heap.len() > self.top_k { heap.pop(); }
                    }
                }
            }
        }

        let mut results: Vec<ShardedSearchResult> = heap
            .into_sorted_vec()
            .into_iter()
            .map(|e| ShardedSearchResult {
                score: e.score, shard_id: e.shard_id, doc_address: e.doc_address,
            })
            .collect();
        results.reverse();

        ctx.metric("total_results", results.len() as f64);
        ctx.set_output("results", PortValue::new(results));
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// build_search_dag — factory
// ---------------------------------------------------------------------------

pub(crate) fn build_search_dag(
    shards: &[Arc<LucivyHandle>],
    shard_pool: &luciole::Pool<ShardMsg>,
    reader_pool: &luciole::Pool<super::sharded_handle::ReaderMsg>,
    router_ref: &luciole::ActorRef<super::sharded_handle::RouterMsg>,
    schema: &Schema,
    query_config: &QueryConfig,
    top_k: usize,
    highlight_sink: Option<Arc<ld_lucivy::query::HighlightSink>>,
) -> Result<Dag, String> {
    let mut dag = Dag::new();
    let num_shards = shards.len();

    // Build the query once to extract prescan params (single source of truth).
    // The query is built again in BuildWeightNode with the same config, but
    // this ensures prescan uses the exact same field/continuation/fuzzy settings.
    let probe_query = crate::query::build_query(
        query_config, schema, &shards[0].index, None,
    )?;
    let prescan_params = probe_query.sfx_prescan_params();

    let needs_prescan = !prescan_params.is_empty();

    // drain → flush
    dag.add_node("drain", DrainNode::new(reader_pool.clone(), router_ref.clone()));
    dag.add_node("flush", FlushNode::new(shards.to_vec(), shard_pool.clone()));
    dag.connect("drain", "done", "flush", "trigger")?;

    // flush → branch: needs prescan?
    dag.add_node("needs_prescan", luciole::BranchNode::new(move || needs_prescan));
    dag.connect("flush", "done", "needs_prescan", "trigger")?;

    // "then" path: prescan_0..N ∥ → merge_prescan → build_weight
    for i in 0..num_shards {
        let node_name = format!("prescan_{i}");
        dag.add_node(&node_name, PrescanShardNode::new(
            shards[i].clone(),
            prescan_params.clone(),
        ));
        dag.connect("needs_prescan", "then", &node_name, "trigger")?;
    }

    dag.add_node("merge_prescan", MergePrescanNode::new(num_shards));
    for i in 0..num_shards {
        dag.connect(
            &format!("prescan_{i}"), "prescan",
            "merge_prescan", &format!("prescan_{i}"),
        )?;
    }

    // build_weight: receives prescan from "then" path OR trigger from "else" path
    dag.add_node("build_weight", BuildWeightNode::new(
        shards.to_vec(),
        schema.clone(),
        shards[0].index.clone(),
        query_config.clone(),
        highlight_sink,
    ));
    dag.connect("merge_prescan", "merged", "build_weight", "prescan")?;
    dag.connect("needs_prescan", "else", "build_weight", "trigger")?;

    // build_weight → search_0..N ∥ (parallel search per shard)
    for i in 0..num_shards {
        dag.add_node(
            &format!("search_{i}"),
            SearchShardNode::new(shard_pool.clone(), i, top_k),
        );
        dag.connect("build_weight", "weight", &format!("search_{i}"), "weight")?;
    }

    // search_0..N → merge_results
    dag.add_node("merge", MergeResultsNode::new(num_shards, top_k));
    for i in 0..num_shards {
        dag.connect(
            &format!("search_{i}"), "hits",
            "merge", &format!("hits_{i}"),
        )?;
    }

    Ok(dag)
}
