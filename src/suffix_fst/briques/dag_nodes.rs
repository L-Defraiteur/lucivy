//! LocalNode implementations for find_literal_v3 DAG.
//!
//! Each node wraps one step of the pipeline. Services = BriquesContext<'a>.
//! No Send, no 'static — borrows from the caller's segment data.

use luciole::local_dag::{LocalNode, LocalNodeCtx};
use luciole::node::PortDef;
use luciole::port::PortType;

use crate::query::posting_resolver::PostingResolver;
use crate::suffix_fst::file_v3::SfxFileReaderV3;

use super::context::BriquesContext;
use crate::suffix_fst::builder::SI0_PREFIX;
use crate::suffix_fst::builder::SI_REST_PREFIX;
use crate::suffix_fst::builder_v3::SI_STRIPPED_PREFIX;

use super::fst_walk::{self, FstCandidateV3, TokenChainV3};
use super::resolve::{self, MatchV3};

// ─── Formatting helpers (explain mode) ──────────────────────────────────

fn format_candidates(candidates: &[FstCandidateV3]) -> String {
    let items: Vec<String> = candidates.iter().map(|c| {
        format!(
            "{{\"sti\":{},\"ord\":{},\"own\":{},\"sep\":{},\"ovl\":{},\"ws\":{}}}",
            c.sti, c.raw_ordinal, c.own_len, c.sep_len, c.overlap_len, c.is_word_start,
        )
    }).collect();
    format!("[{}]", items.join(","))
}

fn format_matches(matches: &[MatchV3]) -> String {
    let items: Vec<String> = matches.iter().map(|m| {
        format!(
            "{{\"doc\":{},\"pos\":{},\"span\":{},\"bytes\":[{},{}]}}",
            m.doc_id, m.position, m.span, m.byte_from, m.byte_to,
        )
    }).collect();
    format!("[{}]", items.join(","))
}

/// Dump all FST keys that start with the query in each partition.
fn dump_fst_keys(reader: &SfxFileReaderV3, query_bytes: &[u8],
    anchor_start: bool, strict_separators: bool) -> String
{
    use lucivy_fst::{IntoStreamer, Streamer};

    let fst = reader.fst();
    let partitions: &[u8] = if anchor_start && strict_separators {
        &[SI0_PREFIX]
    } else if anchor_start && !strict_separators {
        &[SI0_PREFIX, SI_STRIPPED_PREFIX]
    } else if strict_separators {
        &[SI0_PREFIX, SI_REST_PREFIX]
    } else {
        &[SI0_PREFIX, SI_REST_PREFIX, SI_STRIPPED_PREFIX]
    };

    let mut items = Vec::new();
    for &partition in partitions {
        let mut ge_key = vec![partition];
        ge_key.extend_from_slice(query_bytes);
        let mut lt_key = ge_key.clone();
        if let Some(last) = lt_key.last_mut() {
            *last = last.saturating_add(1);
        }

        let mut stream = fst.range().ge(&ge_key).lt(&lt_key).into_stream();
        while let Some((key, val)) = stream.next() {
            // key[0] is partition byte, rest is the text
            let text_bytes = &key[1..];
            let text = String::from_utf8_lossy(text_bytes);
            let parents = reader.decode_parents(val, key);
            let parent_strs: Vec<String> = parents.iter().map(|p| {
                format!("{{\"ord\":{},\"sti\":{},\"own\":{},\"sep\":{},\"ovl\":{},\"ws\":{}}}",
                    p.raw_ordinal, p.sti, p.own_len, p.sep_len, p.overlap_len, p.is_word_start)
            }).collect();
            items.push(format!(
                "{{\"part\":{},\"key\":\"{}\",\"parents\":[{}]}}",
                partition, text, parent_strs.join(","),
            ));
        }
    }
    format!("[{}]", items.join(","))
}

/// Format per-candidate posting details: ordinal → [doc_ids] (capped at 20).
fn format_candidate_postings(candidates: &[FstCandidateV3], resolver: &dyn PostingResolver) -> String {
    let mut items = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for c in candidates {
        if !seen.insert(c.raw_ordinal) { continue; }
        let postings = resolver.resolve(c.raw_ordinal);
        let doc_ids: Vec<u32> = postings.iter().map(|p| p.doc_id).collect();
        let total = doc_ids.len();
        let preview: Vec<String> = doc_ids.iter().take(30).map(|d| d.to_string()).collect();
        items.push(format!(
            "{{\"ord\":{},\"own\":{},\"sep\":{},\"ws\":{},\"total_postings\":{},\"docs\":[{}]{}}}",
            c.raw_ordinal, c.own_len, c.sep_len, c.is_word_start,
            total, preview.join(","),
            if total > 30 { ",\"truncated\":true" } else { "" },
        ));
    }
    format!("[{}]", items.join(","))
}

fn format_chains(chains: &[TokenChainV3]) -> String {
    let items: Vec<String> = chains.iter().map(|c| {
        let ords: Vec<String> = c.ordinals.iter()
            .map(|alts| {
                if alts.len() == 1 {
                    format!("{}", alts[0])
                } else {
                    format!("[{}]", alts.iter().map(|o| o.to_string()).collect::<Vec<_>>().join(","))
                }
            })
            .collect();
        format!(
            "{{\"first_sti\":{},\"consumed\":{},\"ords\":[{}]}}",
            c.first_sti, c.total_query_consumed, ords.join(","),
        )
    }).collect();
    format!("[{}]", items.join(","))
}

// ─── FstCandidatesNode ───────────────────────────────────────────────────

/// Tier 1: walk the FST and produce candidate ordinals.
pub struct FstCandidatesNode {
    /// Literal to look up (lowercased before the walk).
    pub query: String,
    /// Restrict to token starts (partition 0x00, plus 0x02 when separators are relaxed).
    pub anchor_start: bool,
    /// When false, also search the sep-stripped partition (0x02).
    pub strict_separators: bool,
}

impl<'a> LocalNode<BriquesContext<'a>> for FstCandidatesNode {
    fn node_type(&self) -> &'static str { "fst_candidates" }

    fn outputs(&self) -> Vec<PortDef> {
        vec![PortDef::required("candidates", PortType::of::<Vec<FstCandidateV3>>())]
    }

    fn execute(&mut self, svc: &BriquesContext<'a>, ctx: &mut LocalNodeCtx) -> Result<(), String> {
        let candidates = fst_walk::fst_candidates_v3(
            svc.reader, &self.query, self.anchor_start, self.strict_separators,
        );
        ctx.metric("candidates", candidates.len() as f64);
        if ctx.explain() {
            let mut detail = format_candidates(&candidates);

            // Dump raw FST keys that match the query prefix
            let lower = self.query.to_lowercase();
            let fst_keys = dump_fst_keys(svc.reader, lower.as_bytes(), self.anchor_start, self.strict_separators);
            detail = format!("{{\"candidates\":{},\"fst_keys\":{}}}", detail, fst_keys);

            ctx.annotate_output("candidates", detail);
        }
        ctx.set_output("candidates", candidates);
        Ok(())
    }
}

// ─── ResolveSingleNode ───────────────────────────────────────────────────

/// Resolve single-token matches from FST candidates.
pub struct ResolveSingleNode {
    /// Query byte length, so `byte_to` measures the match and not the token.
    pub query_len: u32,
}

impl<'a> LocalNode<BriquesContext<'a>> for ResolveSingleNode {
    fn node_type(&self) -> &'static str { "resolve_single" }

    fn inputs(&self) -> Vec<PortDef> {
        vec![PortDef::required("candidates", PortType::of::<Vec<FstCandidateV3>>())]
    }

    fn outputs(&self) -> Vec<PortDef> {
        vec![PortDef::required("matches", PortType::of::<Vec<MatchV3>>())]
    }

    fn execute(&mut self, svc: &BriquesContext<'a>, ctx: &mut LocalNodeCtx) -> Result<(), String> {
        let explain = ctx.explain();
        let candidates = ctx.input::<Vec<FstCandidateV3>>("candidates")
            .ok_or("missing candidates input")?;
        let matches = resolve::resolve_single_v3(candidates, svc.resolver, svc.filter_docs, self.query_len);
        // Build annotation while we still borrow candidates
        let annotation = if explain {
            let postings_detail = format_candidate_postings(candidates, svc.resolver);
            let matches_json = format_matches(&matches);
            Some(format!("{{\"postings\":{},\"matches\":{}}}", postings_detail, matches_json))
        } else {
            None
        };
        // Now we can mutably borrow ctx
        ctx.metric("matches", matches.len() as f64);
        if let Some(a) = annotation {
            ctx.annotate_output("matches", a);
        }
        ctx.set_output("matches", matches);
        Ok(())
    }
}

// ─── ResolveSingleWordNode ──────────────────────────────────────────────

/// Resolve word-stripped candidates (0x02) directly via WordSfxPost.
/// Symmetric to ResolveSingleNode for chunks — no chain dependency.
pub struct ResolveSingleWordNode {
    /// Query byte length, so `byte_to` measures the match and not the token.
    pub query_len: u32,
}

impl<'a> LocalNode<BriquesContext<'a>> for ResolveSingleWordNode {
    fn node_type(&self) -> &'static str { "resolve_single_word" }

    fn inputs(&self) -> Vec<PortDef> {
        vec![PortDef::required("candidates", PortType::of::<Vec<FstCandidateV3>>())]
    }

    fn outputs(&self) -> Vec<PortDef> {
        vec![PortDef::required("matches", PortType::of::<Vec<MatchV3>>())]
    }

    fn execute(&mut self, svc: &BriquesContext<'a>, ctx: &mut LocalNodeCtx) -> Result<(), String> {
        let candidates = ctx.input::<Vec<FstCandidateV3>>("candidates")
            .ok_or("missing candidates input")?;
        let wsp = svc.require_word_sfxpost();
        let matches = resolve::resolve_single_word_v3(candidates, wsp, svc.filter_docs, self.query_len);
        ctx.metric("matches", matches.len() as f64);
        if ctx.explain() { ctx.annotate_output("matches", format_matches(&matches)); }
        ctx.set_output("matches", matches);
        Ok(())
    }
}

// ─── ChunkChainNode ─────────────────────────────────────────────────────

/// Build cross-chunk chains via falling walk (partitions 0x00 + 0x01).
pub struct ChunkChainNode {
    /// Literal to chain across chunk boundaries.
    pub query: String,
    /// Keep only chains whose first token matches at a token start (`first_sti == 0`).
    pub anchor_start: bool,
}

impl<'a> LocalNode<BriquesContext<'a>> for ChunkChainNode {
    fn node_type(&self) -> &'static str { "chunk_chain" }

    fn outputs(&self) -> Vec<PortDef> {
        vec![PortDef::required("chains", PortType::of::<Vec<TokenChainV3>>())]
    }

    fn execute(&mut self, svc: &BriquesContext<'a>, ctx: &mut LocalNodeCtx) -> Result<(), String> {
        let chains = fst_walk::cross_chunk_chain_v3(svc.reader, &self.query);
        let chains: Vec<_> = if self.anchor_start {
            chains.into_iter().filter(|c| c.first_sti == 0).collect()
        } else {
            chains
        };
        ctx.metric("chains", chains.len() as f64);
        if ctx.explain() { ctx.annotate_output("chains", format_chains(&chains)); }
        ctx.set_output("chains", chains);
        Ok(())
    }
}

// ─── SiblingChunkNode ───────────────────────────────────────────────────

/// Sibling chain supplement for chunk pipeline.
/// Uses falling_walk_chunks + splits_from_fst_candidates + sibling_chain_dfs.
pub struct SiblingChunkNode {
    /// Literal whose splits seed the sibling depth-first search.
    pub query: String,
    /// Under strict separators a chain step must cover the next token's separator
    /// too, not just its content — otherwise a strict search jumps over spaces.
    pub strict_separators: bool,
}

impl<'a> LocalNode<BriquesContext<'a>> for SiblingChunkNode {
    fn node_type(&self) -> &'static str { "sibling_chunk" }

    fn inputs(&self) -> Vec<PortDef> {
        vec![PortDef::required("candidates", PortType::of::<Vec<FstCandidateV3>>())]
    }

    fn outputs(&self) -> Vec<PortDef> {
        vec![PortDef::required("sib_chains", PortType::of::<Vec<TokenChainV3>>())]
    }

    fn execute(&mut self, svc: &BriquesContext<'a>, ctx: &mut LocalNodeCtx) -> Result<(), String> {
        let candidates = ctx.input::<Vec<FstCandidateV3>>("candidates")
            .ok_or("missing candidates input")?;

        let mut all_splits = fst_walk::falling_walk_chunks(svc.reader, &self.query);
        // Only use chunk candidates (partitions 0x00/0x01) for the chunk pipeline.
        // Word-stripped ordinals (0x02) have empty sfxpost → chain resolution would fail.
        let chunk_candidates: Vec<_> = candidates.iter()
            .filter(|c| !c.is_word_stripped())
            .cloned()
            .collect();
        let extra = fst_walk::splits_from_fst_candidates(&chunk_candidates, self.query.to_lowercase().len());
        for s in extra {
            if !s.parent.is_word_start || s.parent.sep_len == 0 { continue; }
            all_splits.push(s);
        }

        let sib_chains = fst_walk::sibling_chain_dfs(
            &all_splits, &self.query,
            svc.require_sibling_v3(), svc.require_termtexts(),
            self.strict_separators, svc.trace_id,
        );
        ctx.metric("sib_chains", sib_chains.len() as f64);
        if ctx.explain() { ctx.annotate_output("sib_chains", format_chains(&sib_chains)); }
        ctx.set_output("sib_chains", sib_chains);
        Ok(())
    }
}

// ─── ResolveChunkNode ───────────────────────────────────────────────────

/// Resolve chunk chains (strict adjacency, pos+1).
/// Merges chunk_chains + sibling_chains before resolving.
pub struct ResolveChunkNode;

impl<'a> LocalNode<BriquesContext<'a>> for ResolveChunkNode {
    fn node_type(&self) -> &'static str { "resolve_chunk" }

    fn inputs(&self) -> Vec<PortDef> {
        vec![
            PortDef::required("chains", PortType::of::<Vec<TokenChainV3>>()),
            PortDef::optional("sib_chains", PortType::of::<Vec<TokenChainV3>>()),
        ]
    }

    fn outputs(&self) -> Vec<PortDef> {
        vec![PortDef::required("matches", PortType::of::<Vec<MatchV3>>())]
    }

    fn execute(&mut self, svc: &BriquesContext<'a>, ctx: &mut LocalNodeCtx) -> Result<(), String> {
        let mut chains = ctx.take_input::<Vec<TokenChainV3>>("chains")
            .ok_or("missing chains input")?;

        if let Some(sib) = ctx.take_input::<Vec<TokenChainV3>>("sib_chains") {
            chains.extend(sib);
        }

        let matches = resolve::resolve_chains_v3(&chains, svc.resolver, svc.filter_docs);
        ctx.metric("chains_total", chains.len() as f64);
        ctx.metric("matches", matches.len() as f64);
        if ctx.explain() { ctx.annotate_output("matches", format_matches(&matches)); }
        ctx.set_output("matches", matches);
        Ok(())
    }
}

// ─── WordChainNode ──────────────────────────────────────────────────────

/// Build cross-word chains via falling walk (partition 0x02).
pub struct WordChainNode {
    /// Literal to chain across word boundaries.
    pub query: String,
    /// Keep only chains whose first token matches at a token start (`first_sti == 0`).
    pub anchor_start: bool,
}

impl<'a> LocalNode<BriquesContext<'a>> for WordChainNode {
    fn node_type(&self) -> &'static str { "word_chain" }

    fn outputs(&self) -> Vec<PortDef> {
        vec![PortDef::required("chains", PortType::of::<Vec<TokenChainV3>>())]
    }

    fn execute(&mut self, svc: &BriquesContext<'a>, ctx: &mut LocalNodeCtx) -> Result<(), String> {
        let chains = fst_walk::cross_word_chain_v3(svc.reader, &self.query);
        let chains: Vec<_> = if self.anchor_start {
            chains.into_iter().filter(|c| c.first_sti == 0).collect()
        } else {
            chains
        };
        ctx.metric("chains", chains.len() as f64);
        if ctx.explain() { ctx.annotate_output("chains", format_chains(&chains)); }
        ctx.set_output("chains", chains);
        Ok(())
    }
}

// ─── SiblingWordNode ────────────────────────────────────────────────────

/// Sibling chain supplement for word pipeline.
pub struct SiblingWordNode {
    /// Literal whose word-level splits seed the sibling depth-first search.
    pub query: String,
}

impl<'a> LocalNode<BriquesContext<'a>> for SiblingWordNode {
    fn node_type(&self) -> &'static str { "sibling_word" }

    fn inputs(&self) -> Vec<PortDef> {
        vec![PortDef::required("candidates", PortType::of::<Vec<FstCandidateV3>>())]
    }

    fn outputs(&self) -> Vec<PortDef> {
        vec![PortDef::required("sib_chains", PortType::of::<Vec<TokenChainV3>>())]
    }

    fn execute(&mut self, svc: &BriquesContext<'a>, ctx: &mut LocalNodeCtx) -> Result<(), String> {
        let candidates = ctx.input::<Vec<FstCandidateV3>>("candidates")
            .ok_or("missing candidates input")?;

        let mut all_splits = fst_walk::falling_walk_words(svc.reader, &self.query);
        let query_len = self.query.to_lowercase().len();
        // Only use word-stripped candidates (partition 0x02) for the word pipeline.
        // Chunk ordinals (0x00/0x01) have empty WordSfxPost → fallback to chunk_resolver
        // in resolve_word_chains_v3 would produce false positives.
        let ws_candidates: Vec<_> = candidates.iter()
            .filter(|c| c.is_word_stripped())
            .cloned()
            .collect();
        let extra = fst_walk::splits_from_fst_candidates(&ws_candidates, query_len);
        for s in extra {
            if s.parent.sep_len == 0 { continue; }
            all_splits.push(s);
        }
        fst_walk::sort_and_dedup_splits(&mut all_splits);

        let sib_chains = fst_walk::sibling_chain_dfs(
            &all_splits, &self.query,
            svc.require_sibling_v3(), svc.require_termtexts(),
            false, // the word pipeline only runs with relaxed separators
            svc.trace_id,
        );
        ctx.metric("splits", all_splits.len() as f64);
        ctx.metric("sib_chains", sib_chains.len() as f64);
        if ctx.explain() { ctx.annotate_output("sib_chains", format_chains(&sib_chains)); }
        ctx.set_output("sib_chains", sib_chains);
        Ok(())
    }
}

// ─── ResolveWordNode ────────────────────────────────────────────────────

/// Resolve word chains (relaxed adjacency via WordSfxPost + posmap + termtexts META).
pub struct ResolveWordNode;

impl<'a> LocalNode<BriquesContext<'a>> for ResolveWordNode {
    fn node_type(&self) -> &'static str { "resolve_word" }

    fn inputs(&self) -> Vec<PortDef> {
        vec![
            PortDef::required("chains", PortType::of::<Vec<TokenChainV3>>()),
            PortDef::optional("sib_chains", PortType::of::<Vec<TokenChainV3>>()),
        ]
    }

    fn outputs(&self) -> Vec<PortDef> {
        vec![PortDef::required("matches", PortType::of::<Vec<MatchV3>>())]
    }

    fn execute(&mut self, svc: &BriquesContext<'a>, ctx: &mut LocalNodeCtx) -> Result<(), String> {
        let mut chains = ctx.take_input::<Vec<TokenChainV3>>("chains")
            .ok_or("missing chains input")?;

        if let Some(sib) = ctx.take_input::<Vec<TokenChainV3>>("sib_chains") {
            chains.extend(sib);
        }

        let pm = svc.require_posmap();
        let tt = svc.require_termtexts();
        let wsp = svc.require_word_sfxpost();
        let matches = resolve::resolve_word_chains_v3(&chains, wsp, svc.resolver, svc.filter_docs, pm, tt);
        ctx.metric("chains_total", chains.len() as f64);
        ctx.metric("matches", matches.len() as f64);
        if ctx.explain() { ctx.annotate_output("matches", format_matches(&matches)); }
        ctx.set_output("matches", matches);
        Ok(())
    }
}

// ─── MergeNode ──────────────────────────────────────────────────────────

/// Merge all match results, sort by (doc_id, position), dedup.
pub struct MergeNode;

impl<'a> LocalNode<BriquesContext<'a>> for MergeNode {
    fn node_type(&self) -> &'static str { "merge" }

    fn inputs(&self) -> Vec<PortDef> {
        vec![
            PortDef::required("single", PortType::of::<Vec<MatchV3>>()),
            PortDef::optional("chunk", PortType::of::<Vec<MatchV3>>()),
            PortDef::optional("single_word", PortType::of::<Vec<MatchV3>>()),
            PortDef::optional("word", PortType::of::<Vec<MatchV3>>()),
        ]
    }

    fn outputs(&self) -> Vec<PortDef> {
        vec![PortDef::required("results", PortType::of::<Vec<MatchV3>>())]
    }

    fn execute(&mut self, _svc: &BriquesContext<'a>, ctx: &mut LocalNodeCtx) -> Result<(), String> {
        let mut results = ctx.take_input::<Vec<MatchV3>>("single")
            .ok_or("missing single input")?;

        if let Some(chunk) = ctx.take_input::<Vec<MatchV3>>("chunk") {
            results.extend(chunk);
        }
        if let Some(single_word) = ctx.take_input::<Vec<MatchV3>>("single_word") {
            results.extend(single_word);
        }
        if let Some(word) = ctx.take_input::<Vec<MatchV3>>("word") {
            results.extend(word);
        }

        results.sort_by_key(|m| (m.doc_id, m.position));

        let unique_docs: std::collections::HashSet<u32> = results.iter().map(|m| m.doc_id).collect();
        ctx.metric("matches", results.len() as f64);
        ctx.metric("unique_docs", unique_docs.len() as f64);
        if ctx.explain() { ctx.annotate_output("results", format_matches(&results)); }
        ctx.set_output("results", results);
        Ok(())
    }
}
