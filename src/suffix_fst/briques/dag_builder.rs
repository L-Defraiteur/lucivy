//! DAG builder for find_literal_v3.
//!
//! Constructs a LocalDag<BriquesContext> with the right nodes and edges
//! based on the query config (strict_separators, has_word_pipeline, has_sibling_chains).

use luciole::dag::DagEdge;
use luciole::local_dag::{EdgeAnnotations, LocalDag};
use luciole::runtime::DagResult;

use super::context::BriquesContext;
use super::dag_nodes::*;
use super::resolve::MatchV3;

/// Result of a DAG-based find_literal_v3 execution.
pub struct LiteralDagResult {
    pub matches: Vec<MatchV3>,
    pub dag_info: DagResult,
    pub edges: Vec<DagEdge>,
    /// Edge annotations (only populated in explain mode).
    pub annotations: EdgeAnnotations,
}

impl LiteralDagResult {
    /// Render the DAG as a Mermaid flowchart with per-node metrics.
    pub fn dump_mermaid(&self) -> String {
        self.dag_info.dump_mermaid(&self.edges)
    }

    /// Dump edge annotations as JSON.
    pub fn dump_edge_data(&self) -> String {
        self.annotations.dump_json()
    }
}

/// Build and execute the find_literal_v3 pipeline as a LocalDag.
pub fn find_literal_v3_dag<'a>(
    ctx: &BriquesContext<'a>,
    query: &str,
    anchor_start: bool,
    strict_separators: bool,
) -> LiteralDagResult {
    let mut dag = build_literal_dag(ctx, query, anchor_start, strict_separators);
    let edges = dag.edges().to_vec();
    let (matches, dag_info) = dag.execute_and_take::<Vec<MatchV3>>(ctx, "merge", "results")
        .expect("find_literal_v3 DAG execution failed");
    LiteralDagResult { matches, dag_info, edges, annotations: EdgeAnnotations::new() }
}

/// Build and execute with explain: nodes annotate their outputs with JSON data.
pub fn find_literal_v3_dag_explained<'a>(
    ctx: &BriquesContext<'a>,
    query: &str,
    anchor_start: bool,
    strict_separators: bool,
) -> LiteralDagResult {
    let mut dag = build_literal_dag(ctx, query, anchor_start, strict_separators);
    let edges = dag.edges().to_vec();
    let (matches, dag_info, annotations) = dag
        .execute_and_take_explained::<Vec<MatchV3>>(ctx, "merge", "results")
        .expect("find_literal_v3 DAG execution failed");
    LiteralDagResult { matches, dag_info, edges, annotations }
}

fn build_literal_dag<'a>(
    ctx: &BriquesContext<'a>,
    query: &str,
    anchor_start: bool,
    strict_separators: bool,
) -> LocalDag<BriquesContext<'a>> {
    let mut dag = LocalDag::new();
    let q = query.to_string();

    // ── Always present ──────────────────────────────────────────
    dag.add_node("fst_candidates", FstCandidatesNode {
        query: q.clone(),
        anchor_start,
        strict_separators,
    });

    dag.add_node("resolve_single", ResolveSingleNode { query_len: q.len() as u32 });
    dag.connect("fst_candidates", "candidates", "resolve_single", "candidates").unwrap();

    // ── Chunk pipeline ──────────────────────────────────────────
    dag.add_node("chunk_chain", ChunkChainNode {
        query: q.clone(),
        anchor_start,
    });

    if ctx.has_sibling_chains() {
        dag.add_node("sibling_chunk", SiblingChunkNode { query: q.clone(), strict_separators });
        dag.connect("fst_candidates", "candidates", "sibling_chunk", "candidates").unwrap();
    }

    dag.add_node("resolve_chunk", ResolveChunkNode);
    dag.connect("chunk_chain", "chains", "resolve_chunk", "chains").unwrap();
    if ctx.has_sibling_chains() {
        dag.connect("sibling_chunk", "sib_chains", "resolve_chunk", "sib_chains").unwrap();
    }

    // ── Word pipeline (conditional) ─────────────────────────────
    let has_word = !strict_separators && ctx.has_word_pipeline();

    if has_word {
        // Direct resolution of word-stripped candidates (no chain dependency)
        dag.add_node("resolve_single_word", ResolveSingleWordNode { query_len: q.len() as u32 });
        dag.connect("fst_candidates", "candidates", "resolve_single_word", "candidates").unwrap();

        dag.add_node("word_chain", WordChainNode {
            query: q.clone(),
            anchor_start,
        });

        if ctx.has_sibling_chains() {
            dag.add_node("sibling_word", SiblingWordNode { query: q.clone() });
            dag.connect("fst_candidates", "candidates", "sibling_word", "candidates").unwrap();
        }

        dag.add_node("resolve_word", ResolveWordNode);
        dag.connect("word_chain", "chains", "resolve_word", "chains").unwrap();
        if ctx.has_sibling_chains() {
            dag.connect("sibling_word", "sib_chains", "resolve_word", "sib_chains").unwrap();
        }
    }

    // ── Merge ───────────────────────────────────────────────────
    dag.add_node("merge", MergeNode);
    dag.connect("resolve_single", "matches", "merge", "single").unwrap();
    dag.connect("resolve_chunk", "matches", "merge", "chunk").unwrap();
    if has_word {
        dag.connect("resolve_single_word", "matches", "merge", "single_word").unwrap();
        dag.connect("resolve_word", "matches", "merge", "word").unwrap();
    }

    dag
}
