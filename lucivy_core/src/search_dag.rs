//! DAG-based search orchestration for sharded indexes.
//!
//! Every step of a sharded search is a DAG node with full observability:
//!
//! ```text
//! drain ── flush ── build_weight ──┬── search_shard_0 ──┐
//!                                  ├── search_shard_1 ──┼── merge_results
//!                                  └── search_shard_2 ──┘
//! ```

use std::collections::BinaryHeap;
use std::sync::Arc;

use luciole::node::{Node, NodeContext, PortDef};
use luciole::port::{PortType, PortValue};
use luciole::Dag;

use ld_lucivy::collector::Collector;
use ld_lucivy::query::Weight;
use ld_lucivy::schema::Schema;
use ld_lucivy::{DocAddress, Index};

use crate::bm25_global::AggregatedBm25StatsOwned;
use crate::handle::LucivyHandle;
use crate::query::QueryConfig;
use crate::sharded_handle::{ShardMsg, ShardedSearchResult, ScoredEntry};

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
// BuildCountWeightNode — pass 1: build Weight with SFX cache (IDF-neutral)
// ---------------------------------------------------------------------------

pub(crate) struct BuildCountWeightNode {
    shards: Vec<Arc<LucivyHandle>>,
    schema: Schema,
    index: Index,
    query_config: QueryConfig,
    sfx_cache: Arc<ld_lucivy::query::SfxCache>,
}

impl BuildCountWeightNode {
    pub fn new(
        shards: Vec<Arc<LucivyHandle>>,
        schema: Schema,
        index: Index,
        query_config: QueryConfig,
        sfx_cache: Arc<ld_lucivy::query::SfxCache>,
    ) -> Self {
        Self { shards, schema, index, query_config, sfx_cache }
    }
}

impl Node for BuildCountWeightNode {
    fn node_type(&self) -> &'static str { "build_count_weight" }
    fn inputs(&self) -> Vec<PortDef> {
        vec![PortDef::trigger("trigger")]
    }
    fn outputs(&self) -> Vec<PortDef> {
        vec![PortDef::required("weight", PortType::of::<Arc<dyn Weight>>())]
    }
    fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
        let searchers: Vec<_> = self.shards.iter().map(|s| s.reader.searcher()).collect();
        let global_stats = AggregatedBm25StatsOwned::new(searchers);

        // Pass 1: build query with cache, no global_doc_freq → IDF-neutral scorers
        let sfx_opts = crate::query::SfxScoringOptions {
            sfx_cache: Some(self.sfx_cache.clone()),
            global_doc_freq: None,
        };
        let query = crate::query::build_query_ex(
            &self.query_config, &self.schema, &self.index, None, Some(&sfx_opts),
        )?;

        let searcher_0 = self.shards[0].reader.searcher();
        let enable_scoring = ld_lucivy::query::EnableScoring::enabled_from_statistics_provider(
            &global_stats, &searcher_0,
        );
        let weight: Arc<dyn Weight> = query
            .weight(enable_scoring)
            .map_err(|e| format!("weight: {e}"))?
            .into();

        ctx.set_output("weight", PortValue::new(weight));
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// BuildScoreWeightNode — pass 2: build Weight with cache + global doc_freq
// ---------------------------------------------------------------------------

pub(crate) struct BuildScoreWeightNode {
    shards: Vec<Arc<LucivyHandle>>,
    schema: Schema,
    index: Index,
    query_config: QueryConfig,
    highlight_sink: Option<Arc<ld_lucivy::query::HighlightSink>>,
    sfx_cache: Arc<ld_lucivy::query::SfxCache>,
    num_count_shards: usize,
}

impl BuildScoreWeightNode {
    pub fn new(
        shards: Vec<Arc<LucivyHandle>>,
        schema: Schema,
        index: Index,
        query_config: QueryConfig,
        highlight_sink: Option<Arc<ld_lucivy::query::HighlightSink>>,
        sfx_cache: Arc<ld_lucivy::query::SfxCache>,
        num_count_shards: usize,
    ) -> Self {
        Self { shards, schema, index, query_config, highlight_sink, sfx_cache, num_count_shards }
    }
}

impl Node for BuildScoreWeightNode {
    fn node_type(&self) -> &'static str { "build_score_weight" }
    fn inputs(&self) -> Vec<PortDef> {
        (0..self.num_count_shards)
            .map(|i| PortDef::trigger(
                Box::leak(format!("counted_{}", i).into_boxed_str()),
            ))
            .collect()
    }
    fn outputs(&self) -> Vec<PortDef> {
        vec![PortDef::required("weight", PortType::of::<Arc<dyn Weight>>())]
    }
    fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
        // Read aggregated doc_freq from cache (populated by count pass)
        let global_doc_freq = self.sfx_cache.doc_freq_count
            .load(std::sync::atomic::Ordering::Relaxed);

        let searchers: Vec<_> = self.shards.iter().map(|s| s.reader.searcher()).collect();
        let global_stats = AggregatedBm25StatsOwned::new(searchers);

        // Pass 2: build query with cache + global_doc_freq → correct IDF
        let sfx_opts = crate::query::SfxScoringOptions {
            sfx_cache: Some(self.sfx_cache.clone()),
            global_doc_freq: Some(global_doc_freq),
        };
        let query = crate::query::build_query_ex(
            &self.query_config, &self.schema, &self.index,
            self.highlight_sink.clone(), Some(&sfx_opts),
        )?;

        let searcher_0 = self.shards[0].reader.searcher();
        let enable_scoring = ld_lucivy::query::EnableScoring::enabled_from_statistics_provider(
            &global_stats, &searcher_0,
        );
        let weight: Arc<dyn Weight> = query
            .weight(enable_scoring)
            .map_err(|e| format!("weight: {e}"))?
            .into();

        ctx.metric("global_doc_freq", global_doc_freq as f64);
        ctx.set_output("weight", PortValue::new(weight));
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// CountShardNode — pass 1: SFX walk on one shard (populates cache, no results)
// ---------------------------------------------------------------------------

pub(crate) struct CountShardNode {
    handle: Arc<LucivyHandle>,
    shard_id: usize,
}

impl CountShardNode {
    pub fn new(handle: Arc<LucivyHandle>, shard_id: usize) -> Self {
        Self { handle, shard_id }
    }
}

impl Node for CountShardNode {
    fn node_type(&self) -> &'static str { "count_shard" }
    fn inputs(&self) -> Vec<PortDef> {
        vec![PortDef::required("weight", PortType::of::<Arc<dyn Weight>>())]
    }
    fn outputs(&self) -> Vec<PortDef> {
        vec![PortDef::trigger("done")]
    }
    fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
        let weight = ctx.input("weight")
            .ok_or("missing weight")?
            .downcast::<Arc<dyn Weight>>()
            .ok_or("wrong weight type")?
            .clone();

        // Call scorer() on each segment — this triggers the SFX walk
        // and populates the SfxCache. The scorer returns EmptyScorer (no results).
        let searcher = self.handle.reader.searcher();
        for (seg_ord, seg_reader) in searcher.segment_readers().iter().enumerate() {
            let _scorer = weight.scorer(seg_reader, 1.0)
                .map_err(|e| format!("count shard_{} seg_{}: {e}", self.shard_id, seg_ord))?;
        }

        ctx.trigger("done");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// ScoreShardNode — pass 2: score from cached SFX walk results
// ---------------------------------------------------------------------------

pub(crate) struct ScoreShardNode {
    handle: Arc<LucivyHandle>,
    shard_id: usize,
    top_k: usize,
}

impl ScoreShardNode {
    pub fn new(handle: Arc<LucivyHandle>, shard_id: usize, top_k: usize) -> Self {
        Self { handle, shard_id, top_k }
    }
}

impl Node for ScoreShardNode {
    fn node_type(&self) -> &'static str { "score_shard" }
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

        // Execute weight directly on handle (same reader as CountShardNode)
        let searcher = self.handle.reader.searcher();
        let collector = ld_lucivy::collector::TopDocs::with_limit(self.top_k).order_by_score();
        let segment_readers = searcher.segment_readers();
        let mut fruits = Vec::with_capacity(segment_readers.len());
        for (seg_ord, seg_reader) in segment_readers.iter().enumerate() {
            let fruit = collector
                .collect_segment(weight.as_ref(), seg_ord as u32, seg_reader)
                .map_err(|e| format!("score shard_{} seg_{}: {e}", self.shard_id, seg_ord))?;
            fruits.push(fruit);
        }
        let hits = collector
            .merge_fruits(fruits)
            .map_err(|e| format!("merge shard_{}: {e}", self.shard_id))?;

        let tagged: Vec<(usize, f32, DocAddress)> = hits.into_iter()
            .map(|(score, addr)| (self.shard_id, score, addr))
            .collect();

        ctx.metric("hits", tagged.len() as f64);
        ctx.set_output("hits", PortValue::new(tagged));
        Ok(())
    }
}

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
    let sfx_cache = Arc::new(ld_lucivy::query::SfxCache::default());

    // drain → flush
    dag.add_node("drain", DrainNode::new(reader_pool.clone(), router_ref.clone()));
    dag.add_node("flush", FlushNode::new(shards.to_vec(), shard_pool.clone()));
    dag.connect("drain", "done", "flush", "trigger")?;

    // Pass 1: count (SFX walk → cache doc_tf, accumulate doc_freq)
    dag.add_node("build_count_weight", BuildCountWeightNode::new(
        shards.to_vec(), schema.clone(), shards[0].index.clone(),
        query_config.clone(), sfx_cache.clone(),
    ));
    dag.connect("flush", "done", "build_count_weight", "trigger")?;

    for i in 0..num_shards {
        let name = format!("count_{}", i);
        dag.add_node(&name, CountShardNode::new(shards[i].clone(), i));
        dag.connect("build_count_weight", "weight", &name, "weight")?;
    }

    // Pass 2: score (read cached doc_tf, use global doc_freq for IDF)
    dag.add_node("build_score_weight", BuildScoreWeightNode::new(
        shards.to_vec(), schema.clone(), shards[0].index.clone(),
        query_config.clone(), highlight_sink, sfx_cache.clone(),
        num_shards,
    ));
    // build_score_weight waits for ALL count nodes to finish
    for i in 0..num_shards {
        dag.connect(
            &format!("count_{}", i), "done",
            "build_score_weight", &format!("counted_{}", i),
        )?;
    }

    for i in 0..num_shards {
        let name = format!("search_{}", i);
        dag.add_node(&name, ScoreShardNode::new(shards[i].clone(), i, top_k));
        dag.connect("build_score_weight", "weight", &name, "weight")?;
    }

    // Merge results
    dag.add_node("merge", MergeResultsNode::new(num_shards, top_k));
    for i in 0..num_shards {
        dag.connect(
            &format!("search_{}", i), "hits",
            "merge", &format!("hits_{}", i),
        )?;
    }

    Ok(dag)
}
