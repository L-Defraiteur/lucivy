//! Tier 3 — Composite operations for SFX v3.
//!
//! High-level building blocks that compose Tier 1 (FST walk) and Tier 2 (resolve):
//!
//! - `find_literal_v3`: find all occurrences of a literal (single + cross-token)
//! - `find_multi_token_v3`: multi-token adjacency search with pivot optimization
//! - `resolve_trigrams_v3`: fuzzy trigram pigeonhole pipeline

use std::collections::HashSet;

use common::BitSet;

use crate::DocId;
use crate::tokenizer::equal_chunk::is_content_char;
use crate::query::posting_resolver::PostingResolver;
use crate::suffix_fst::file_v3::SfxFileReaderV3;

use super::context::BriquesContext;
use super::fst_walk::{self, FstCandidateV3, TokenChainV3};
use super::resolve::{self, MatchV3};
use super::profile;

// ─── find_literal_v3 ──────────────────────────────────────────────────────

/// Find all occurrences of a literal string.
///
/// Two separate pipelines:
/// - **Chunk pipeline** (0x00/0x01): fst_candidates + cross_chunk_chain → strict resolve (pos+1)
/// - **Word pipeline** (0x02): fst_candidates + cross_word_chain → relaxed resolve (posmap/bytemap)
///
/// No mixing between pipelines. No fallbacks — posmap/bytemap are REQUIRED
/// for the word pipeline. Without them, only chunk pipeline runs.
pub fn find_literal_v3(
    ctx: &BriquesContext<'_>,
    query: &str,
    anchor_start: bool,
    strict_separators: bool,
) -> Vec<MatchV3> {
    let mut results = Vec::new();

    ctx.trace_enter("find_literal_v3");
    ctx.trace_msg(&format!("params query={} strict={} anchor={}", query, strict_separators, anchor_start));
    let dbg = std::env::var("V3_DIAG_LITERAL").ok().as_deref() == Some(query);

    // ── Single-token matches (all partitions) ────────────────────────
    let _t = profile::Timer::start();
    let candidates = fst_walk::fst_candidates_v3(ctx.reader, query, anchor_start, strict_separators);
    let query_len = query.len() as u32;
    let single = resolve::resolve_single_v3(&candidates, ctx.resolver, ctx.filter_docs, query_len);
    _t.stop(|c| &c.ns_single);
    ctx.trace_msg(&format!("single_token candidates={} matches={}", candidates.len(), single.len()));
    if ctx.trace_id.is_some() {
        if candidates.len() < 50 {
            for c in &candidates {
                let postings = ctx.resolver.resolve(c.raw_ordinal);
                ctx.trace_msg(&format!("  cand sti={} ord={} own={} sep={} ovl={} postings={}",
                    c.sti, c.raw_ordinal, c.own_len, c.sep_len, c.overlap_len, postings.len()));
            }
        }
        // Log unique doc_ids from single-token matches
        let docs: std::collections::HashSet<u32> = single.iter().map(|m| m.doc_id).collect();
        ctx.trace_msg(&format!("  single_docs: {} unique", docs.len()));
    }
    if dbg { eprintln!("[lit] {query:?} strict={strict_separators} single={}", single.len()); }
    results.extend(single);

    // Word-stripped singles (0x02). resolve_single_v3 skips them — their postings
    // live in WordSfxPost, not sfxpost — so without this the production path had no
    // direct resolution for that partition at all and depended entirely on chains
    // catching the case. The DAG and the fuzzy pipeline already resolved it; only
    // find_literal_v3 did not.
    if ctx.has_word_pipeline() {
        let single_word = resolve::resolve_single_word_v3(
            &candidates, ctx.require_word_sfxpost(), ctx.filter_docs, query_len,
        );
        ctx.trace_msg(&format!("single_word matches={}", single_word.len()));
        results.extend(single_word);
    }

    // ── Chunk chains (0x00 + 0x01) — strict adjacency ────────────────
    {
        let _t = profile::Timer::start();
        let mut chains = fst_walk::cross_chunk_chain_v3(ctx.reader, query);
        _t.stop(|c| &c.ns_chunk_walk);

        // Sibling chain supplement: if sibling table is available, use it
        // for continuations. Also catches first splits missed by falling walk.
        if ctx.has_sibling_chains() {
            let _t = profile::Timer::start();
            let mut all_splits = fst_walk::falling_walk_chunks(ctx.reader, query);
            let extra = fst_walk::splits_from_fst_candidates(&candidates, query.to_lowercase().len());
            // Only chunk candidates (sti-based, non-word-stripped)
            for s in extra {
                if !s.parent.is_word_start || s.parent.sep_len == 0 { continue; }
                all_splits.push(s);
            }
            let sib_chains = fst_walk::sibling_chain_dfs(
                &all_splits, query,
                ctx.require_sibling_v3(), ctx.require_termtexts(),
                strict_separators, ctx.trace_id,
            );
            chains.extend(sib_chains);
            _t.stop(|c| &c.ns_chunk_sibling);
        }

        let chains: Vec<_> = if anchor_start {
            chains.into_iter().filter(|c| c.first_sti == 0).collect()
        } else {
            chains
        };
        ctx.trace_msg(&format!("chunk_chains falling_walk={}", chains.len()));
        profile::bump(|c| &c.n_chunk_chains, chains.len() as u64);
        profile::bump(|c| &c.n_chains_raw, chains.len() as u64);
        let _t = profile::Timer::start();
        let cross = match ctx.posmap.as_ref() {
            Some(pm) => resolve::resolve_chains_v3_posmap(
                &chains, ctx.resolver, ctx.filter_docs, pm),
            None => resolve::resolve_chains_v3(&chains, ctx.resolver, ctx.filter_docs),
        };
        _t.stop(|c| &c.ns_chunk_resolve);
        ctx.trace_msg(&format!("chunk_resolved matches={}", cross.len()));
        if dbg {
            eprintln!("[lit]   chunk_chains={} -> matches={}", chains.len(), cross.len());
            for c in chains.iter().take(4) {
                let texts: Vec<String> = c.ordinals.iter()
                    .map(|alts| alts.iter().take(2)
                        .filter_map(|&o| ctx.termtexts.as_ref().and_then(|t| t.text(o as u32)))
                        .collect::<Vec<_>>().join("|"))
                    .collect();
                eprintln!("[lit]     chain sti={} consumed={} last={} tokens={:?}",
                    c.first_sti, c.total_query_consumed, c.last_consumed, texts);
            }
        }
        results.extend(cross);
    }

    // ── Word chains (0x02) — relaxed adjacency via WordSfxPost ─────
    if !strict_separators && ctx.has_word_pipeline() {
        let pm = ctx.require_posmap();
        let bm = ctx.require_bytemap();
        let wsp = ctx.require_word_sfxpost();

        let _t = profile::Timer::start();
        let mut chains = fst_walk::cross_word_chain_v3(ctx.reader, query);
        _t.stop(|c| &c.ns_word_walk);
        ctx.trace_msg(&format!("word_falling_walk chains={}", chains.len()));

        // Sibling chain supplement for word pipeline
        if ctx.has_sibling_chains() {
            let _t = profile::Timer::start();
            let mut all_splits = fst_walk::falling_walk_words(ctx.reader, query);
            let query_len = query.to_lowercase().len();
            let extra = fst_walk::splits_from_fst_candidates(&candidates, query_len);
            ctx.trace_msg(&format!("word_splits falling_walk={} fst_cand={}", all_splits.len(), extra.len()));
            for s in extra {
                if s.parent.sep_len == 0 { continue; }
                all_splits.push(s);
            }
            fst_walk::sort_and_dedup_splits(&mut all_splits);
            ctx.trace_msg(&format!("word_splits_merged total={}", all_splits.len()));
            let sib_chains = fst_walk::sibling_chain_dfs(
                &all_splits, query,
                ctx.require_sibling_v3(), ctx.require_termtexts(),
                strict_separators, ctx.trace_id,
            );
            ctx.trace_msg(&format!("word_sibling_chains count={}", sib_chains.len()));
            chains.extend(sib_chains);
            _t.stop(|c| &c.ns_word_sibling);
        }

        let chains: Vec<_> = if anchor_start {
            chains.into_iter().filter(|c| c.first_sti == 0).collect()
        } else {
            chains
        };
        ctx.trace_msg(&format!("word_chains_total count={}", chains.len()));
        profile::bump(|c| &c.n_word_chains, chains.len() as u64);
        let _t = profile::Timer::start();
        let cross = resolve::resolve_word_chains_v3(&chains, wsp, ctx.resolver, ctx.filter_docs, pm, bm);
        _t.stop(|c| &c.ns_word_resolve);
        ctx.trace_msg(&format!("word_resolved matches={}", cross.len()));
        results.extend(cross);
    }

    results.sort_by_key(|m| (m.doc_id, m.position));
    profile::bump(|c| &c.n_matches_emitted, results.len() as u64);
    let unique_docs: std::collections::HashSet<u32> = results.iter().map(|m| m.doc_id).collect();
    ctx.trace_msg(&format!("total matches={} unique_docs={}", results.len(), unique_docs.len()));
    ctx.trace_exit();
    results
}

// ─── find_multi_token_v3 ──────────────────────────────────────────────────

/// Multi-token adjacency search with pivot optimization.
///
/// Splits the query on non-alphanum boundaries, resolves each sub-token
/// independently, picks the most selective as pivot, then verifies
/// adjacency bidirectionally.
pub fn find_multi_token_v3(
    ctx: &BriquesContext<'_>,
    query_tokens: &[&str],
    anchor_start: bool,
    _exact_match: bool,
    strict_separators: bool,
) -> Vec<MatchV3> {
    if query_tokens.is_empty() {
        return Vec::new();
    }
    if query_tokens.len() == 1 {
        return find_literal_v3(ctx, query_tokens[0], anchor_start, strict_separators);
    }

    // Resolve each sub-token independently
    let per_token: Vec<Vec<MatchV3>> = query_tokens
        .iter()
        .enumerate()
        .map(|(i, token)| {
            let anchor = anchor_start && i == 0;
            find_literal_v3(ctx, token, anchor, strict_separators)
        })
        .collect();

    // Pick pivot: the sub-token with fewest matches (most selective)
    let pivot_idx = per_token
        .iter()
        .enumerate()
        .min_by_key(|(_, matches)| matches.len())
        .map(|(i, _)| i)
        .unwrap_or(0);

    // From pivot, verify adjacency backward and forward
    let mut results = Vec::new();

    for pivot_match in &per_token[pivot_idx] {
        let doc_id = pivot_match.doc_id;
        let pivot_pos = pivot_match.position;

        // Check backward: tokens before pivot must be at consecutive positions
        let mut valid = true;
        let mut byte_from = pivot_match.byte_from;

        for step in (0..pivot_idx).rev() {
            let expected_pos = pivot_pos - (pivot_idx - step) as u32;
            let found = per_token[step]
                .iter()
                .any(|m| m.doc_id == doc_id && m.position == expected_pos);
            if !found {
                valid = false;
                break;
            }
            if let Some(m) = per_token[step]
                .iter()
                .find(|m| m.doc_id == doc_id && m.position == expected_pos)
            {
                byte_from = m.byte_from;
            }
        }

        if !valid {
            continue;
        }

        // Check forward: tokens after pivot must be at consecutive positions
        let mut byte_to = pivot_match.byte_to;
        let mut token_end = pivot_match.token_end;

        for step in (pivot_idx + 1)..query_tokens.len() {
            let expected_pos = pivot_pos + (step - pivot_idx) as u32;
            let found = per_token[step]
                .iter()
                .any(|m| m.doc_id == doc_id && m.position == expected_pos);
            if !found {
                valid = false;
                break;
            }
            if let Some(m) = per_token[step]
                .iter()
                .find(|m| m.doc_id == doc_id && m.position == expected_pos)
            {
                byte_to = m.byte_to;
                token_end = m.token_end;
            }
        }

        if valid {
            results.push(MatchV3 {
                doc_id,
                position: pivot_pos - pivot_idx as u32,
                span: query_tokens.len() as u32,
                byte_from,
                byte_to,
                token_end,
                sti: 0,
                ordinal: pivot_match.ordinal,
                last_ordinal: pivot_match.ordinal,
            });
        }
    }

    // Dedup
    results.sort_by_key(|m| (m.doc_id, m.position));
    results.dedup_by_key(|m| (m.doc_id, m.position));

    results
}

// ─── Fuzzy briques ───────────────────────────────────────────────────────

/// A single trigram hit in a document.
#[derive(Debug, Clone)]
pub struct TrigramHit {
    pub tri_idx: usize,
    pub doc_id: DocId,
    pub position: u32,
    pub byte_from: u32,
    pub byte_to: u32,
}

/// Generate n-grams from query text with their position in the query.
/// n=2 if query is short (len ≤ 3*(distance+1)), n=3 otherwise.
/// Returns (ngrams, query_positions, n) where query_positions[i] = byte offset
/// of ngram i within the query. Used for ordered matching.
fn generate_trigrams(query: &str, distance: u8) -> (Vec<String>, Vec<usize>, usize) {
    let lower = query.to_lowercase();
    let bytes = lower.as_bytes();
    let n = if bytes.len() <= 3 * (distance as usize + 1) { 2 } else { 3 };

    let mut ngrams = Vec::new();
    let mut positions = Vec::new();
    if bytes.len() < n {
        return (ngrams, positions, n);
    }
    for i in 0..=bytes.len() - n {
        if !lower.is_char_boundary(i) || !lower.is_char_boundary(i + n) {
            continue;
        }
        ngrams.push(lower[i..i + n].to_string());
        positions.push(i);
    }
    (ngrams, positions, n)
}

// ─── Brique 2: resolve_all_trigrams ──────────────────────────────────────

/// Resolve all trigrams against the index, returning hits.
/// Uses both chunk (0x00/0x01) and word-stripped (0x02) resolution.
/// Trigrams are resolved rarest-first (by FST selectivity) for efficiency.
pub fn resolve_all_trigrams(
    ctx: &BriquesContext<'_>,
    ngrams: &[String],
    strict_separators: bool,
) -> Vec<TrigramHit> {
    let mut selectivity: Vec<(usize, usize)> = ngrams.iter().enumerate()
        .map(|(i, gram)| {
            let count = fst_walk::fst_candidates_v3(ctx.reader, gram, false, strict_separators).len();
            (i, count)
        }).collect();
    selectivity.sort_by_key(|&(_, count)| count);

    let has_wsp = ctx.has_word_pipeline();
    let mut all_hits = Vec::new();

    for &(gram_idx, _) in &selectivity {
        let cands = fst_walk::fst_candidates_v3(ctx.reader, &ngrams[gram_idx], false, strict_separators);
        let gram_len = ngrams[gram_idx].len() as u32;
        let chunk_matches = resolve::resolve_single_v3(&cands, ctx.resolver, None, gram_len);
        let word_matches = if has_wsp {
            resolve::resolve_single_word_v3(&cands, ctx.require_word_sfxpost(), None, gram_len)
        } else { Vec::new() };

        for m in chunk_matches.iter().chain(word_matches.iter()) {
            all_hits.push(TrigramHit {
                tri_idx: gram_idx, doc_id: m.doc_id, position: m.position,
                byte_from: m.byte_from, byte_to: m.byte_to,
            });
        }
    }
    all_hits
}

// ─── Brique 3: build_trigram_chains ─────────────────────────────────────

/// A chain of adjacent trigram hits in a document.
#[derive(Debug, Clone)]
pub struct TrigramChain {
    pub doc_id: DocId,
    /// Trigram indices in chain order (matching query order).
    pub trigram_indices: Vec<usize>,
    /// Byte range of the chain.
    pub byte_from: u32,
    pub byte_to: u32,
    /// Token positions of the chain ends. Byte offsets alone are not enough to
    /// rebuild the source text: posmap is keyed by position, not by byte.
    pub first_pos: u32,
    pub last_pos: u32,
}

/// How many raw bytes of separators a chain step may span beyond the query gap.
///
/// Only meaningful because the query is compared in stripped space while hits are
/// in raw space. Loose retrieval is safe here: `verify_candidates` re-checks every
/// surviving document against the real text.
const MAX_SEPARATOR_SLACK: i32 = 8;

/// How many distinct chains we keep per document.
const MAX_CHAINS_PER_DOC: usize = 8;

/// Build chains of adjacent trigram hits per document.
///
/// For each doc, sorts hits by byte_from. Builds chains where consecutive
/// trigrams satisfy:
/// - query_positions[next] > query_positions[prev] (correct order)
/// - byte_from[next] - byte_from[prev] == query_positions[next] - query_positions[prev] ± distance
///
/// This is much more selective than windowed counting because it verifies
/// that trigrams form a coherent subsequence at the correct relative offsets.
pub fn build_trigram_chains(
    hits: &[TrigramHit],
    query_positions: &[usize],
    distance: u8,
) -> Vec<TrigramChain> {
    let d = distance as i32;

    let mut hits_by_doc: std::collections::HashMap<DocId, Vec<&TrigramHit>> =
        std::collections::HashMap::new();
    for hit in hits {
        hits_by_doc.entry(hit.doc_id).or_default().push(hit);
    }

    let mut chains = Vec::new();

    for (&doc_id, doc_hits) in &hits_by_doc {
        let mut sorted: Vec<&TrigramHit> = doc_hits.iter().copied().collect();
        sorted.sort_by_key(|h| h.byte_from);

        // Keep several chains per doc, not just the longest one.
        //
        // Anchoring a document on its single best chain means a longer decoy hides a
        // real match elsewhere in the same file — and the real one is often exactly
        // at the threshold. `retrun` chains only 3 bigrams over
        // "asse|rt run|scripts" (tr, ru, un), so any 4-bigram coincidence elsewhere
        // in the document evicted it. Only worth doing together with the separator
        // slack above: without it that chain never formed in the first place.
        // It also unpins doc_tf, which was stuck at 1 for every document.
        let mut found: Vec<(Vec<usize>, u32, u32, u32, u32)> = Vec::new();

        for start in 0..sorted.len() {
            let mut chain = vec![sorted[start].tri_idx];
            let mut prev_bf = sorted[start].byte_from as i32;
            let mut prev_qp = query_positions[sorted[start].tri_idx] as i32;
            let mut chain_last_pos = sorted[start].position;
            let mut last_bt = sorted[start].byte_to;

            for j in (start + 1)..sorted.len() {
                let h = sorted[j];
                let qp = query_positions[h.tri_idx] as i32;
                if qp <= prev_qp { continue; }

                let expected_gap = qp - prev_qp;
                let actual_gap = h.byte_from as i32 - prev_bf;
                let delta = actual_gap - expected_gap;
                if delta >= -d && delta <= d + MAX_SEPARATOR_SLACK {
                    chain.push(h.tri_idx);
                    prev_bf = h.byte_from as i32;
                    prev_qp = qp;
                    chain_last_pos = h.position;
                    last_bt = h.byte_to;
                }
            }

            found.push((chain, sorted[start].byte_from, last_bt,
                        sorted[start].position, chain_last_pos));
        }

        // Longest first, then a bounded, distinct sample: wide enough to cover
        // several locations, bounded so a hit-dense document cannot explode the
        // candidate set. Verification prunes whatever survives.
        found.sort_by(|a, b| b.0.len().cmp(&a.0.len()));
        let mut seen_starts: HashSet<u32> = HashSet::new();
        for (chain, bf, bt, first_pos, last_pos) in found {
            if seen_starts.len() >= MAX_CHAINS_PER_DOC { break; }
            if !seen_starts.insert(bf) { continue; }
            chains.push(TrigramChain {
                doc_id,
                trigram_indices: chain,
                byte_from: bf,
                byte_to: bt.max(bf),
                first_pos,
                last_pos: last_pos.max(first_pos),
            });
        }
    }

    chains
}


// ─── Brique 5: verify_candidate ─────────────────────────────────────────

/// Does `window` contain a substring within edit distance `d` of `query`?
///
/// Semi-global DP: row 0 is all zeros, so a match may start anywhere; the answer
/// is the minimum of the last row. Two rows of `window.len()+1` cells, reused by
/// the caller — no allocation per candidate.
fn within_edit_distance(query: &[u8], window: &[u8], d: usize, buf: &mut Vec<u32>) -> bool {
    let n = window.len();
    if query.is_empty() { return true; }
    buf.clear();
    buf.resize(2 * (n + 1), 0);
    let (mut cur, mut prev) = (0usize, n + 1);
    for j in 0..=n { buf[prev + j] = 0; }

    for (i, &qb) in query.iter().enumerate() {
        buf[cur] = i as u32 + 1;
        let mut row_min = buf[cur];
        for j in 1..=n {
            let cost = u32::from(qb != window[j - 1]);
            let v = (buf[prev + j] + 1)
                .min(buf[cur + j - 1] + 1)
                .min(buf[prev + j - 1] + cost);
            buf[cur + j] = v;
            row_min = row_min.min(v);
        }
        // Every remaining query byte can only add to the distance.
        if row_min > d as u32 + (query.len() - i - 1) as u32 { return false; }
        std::mem::swap(&mut cur, &mut prev);
    }
    (0..=n).any(|j| buf[prev + j] <= d as u32)
}

/// Rebuild the source text around a trigram chain, bounded by the query length.
///
/// Zero-copy on termtexts (`text()` returns `&str`) into a caller-owned buffer:
/// the fuzzy candidate set can be large, so an allocation per token would be felt.
/// Each token contributes `text[..own_len]` — the overlap tail belongs to the next
/// token and would be duplicated.
pub(super) fn rebuild_window(
    ctx: &BriquesContext<'_>,
    doc_id: DocId,
    first_pos: u32,
    last_pos: u32,
    margin: u32,
    strip_separators: bool,
    out: &mut String,
) -> bool {
    let (Some(pm), Some(tt)) = (ctx.posmap.as_ref(), ctx.termtexts.as_ref()) else {
        return false;
    };
    out.clear();
    let from = first_pos.saturating_sub(margin);
    let to = last_pos.saturating_add(margin);
    for pos in from..=to {
        let Some(ord) = pm.ordinal_at(doc_id, pos) else { break };
        let Some(text) = tt.text(ord) else { break };
        let own = tt.meta(ord).map(|m| m.own_len as usize).unwrap_or(text.len());
        let end = own.min(text.len());
        // Lowercase to match the index: FST keys are built lowercased and the
        // query arrives lowercased, so the engine is case-insensitive for fuzzy.
        // termtexts keeps the ORIGINAL case, so comparing raw bytes here rejects
        // "Functions" for the query "functin" — a match the index did find.
        for c in text[..end].chars() {
            if strip_separators && !is_content_char(c) { continue; }
            for lc in c.to_lowercase() { out.push(lc); }
        }
    }
    !out.is_empty()
}

// ─── Brique 4: filter_by_chain_threshold ────────────────────────────────

/// Filter chains by minimum length (pigeonhole threshold).
/// Returns accepted doc_ids with their highlight spans and coverage scores.
pub fn filter_by_chain_threshold(
    chains: &[TrigramChain],
    threshold: usize,
    total_trigrams: usize,
    max_doc: DocId,
) -> (BitSet, Vec<(DocId, usize, usize)>, Vec<(DocId, f32)>) {
    let mut bitset = BitSet::with_max_value(max_doc);
    let mut highlights = Vec::new();
    let mut coverage = Vec::new();

    for chain in chains {
        if chain.trigram_indices.len() >= threshold {
            bitset.insert(chain.doc_id);
            highlights.push((chain.doc_id, chain.byte_from as usize, chain.byte_to as usize));
            let miss_count = total_trigrams - chain.trigram_indices.len();
            coverage.push((chain.doc_id, -(miss_count as f32)));
        }
    }

    (bitset, highlights, coverage)
}

// ─── resolve_trigrams_v3 (composed from briques) ────────────────────────

/// Fuzzy trigram pigeonhole resolution.
///
/// Composed from briques:
/// 1. generate_trigrams — extract n-grams with query positions
/// 2. resolve_all_trigrams — resolve each via chunk + word-stripped
/// 3. build_trigram_chains — adjacency-checked chains per doc
/// 4. filter_by_chain_threshold — pigeonhole filter
///
/// Returns: (doc_bitset, highlights, doc_coverage)
pub fn resolve_trigrams_v3(
    ctx: &BriquesContext<'_>,
    query: &str,
    distance: u8,
    strict_separators: bool,
    max_doc: DocId,
) -> (BitSet, Vec<(DocId, usize, usize)>, Vec<(DocId, f32)>) {
    let (ngrams, query_positions, n) = generate_trigrams(query, distance);
    if ngrams.is_empty() {
        return (BitSet::with_max_value(max_doc), Vec::new(), Vec::new());
    }

    // Floor of 1, not 2.
    //
    // The threshold used to be the only thing standing between the pigeonhole and
    // the result set, so it had to be defensive — and being defensive on a
    // necessary-but-insufficient condition buys false negatives, not precision.
    // Now that verify_candidates re-checks every survivor against the text, the
    // threshold is purely a recall/cost knob: lowering it can only add candidates,
    // and every added candidate is exactly checked.
    let threshold = (ngrams.len() as i32 - n as i32 * distance as i32).max(1) as usize;
    let hits = resolve_all_trigrams(ctx, &ngrams, strict_separators);
    let chains = build_trigram_chains(&hits, &query_positions, distance);
    let (bitset, highlights, coverage) =
        filter_by_chain_threshold(&chains, threshold, ngrams.len(), max_doc);

    // The pigeonhole threshold is a NECESSARY condition and never a sufficient one:
    // it says "enough n-grams of the query appear here", not "some substring is
    // within edit distance d". Raising it only trades false positives for false
    // negatives. The only way to reach zero FP is to check the text — which posmap
    // and termtexts already allow, without touching the docstore.
    verify_candidates(
        ctx, query, distance, strict_separators, max_doc,
        &chains, threshold, bitset, highlights, coverage,
    )
}

/// Drop candidates whose text holds no substring within `distance` of the query.
///
/// Skipped entirely when posmap or termtexts are absent — the pipeline then keeps
/// its previous, permissive behaviour rather than silently dropping everything.
#[allow(clippy::too_many_arguments)]
fn verify_candidates(
    ctx: &BriquesContext<'_>,
    query: &str,
    distance: u8,
    strict_separators: bool,
    max_doc: DocId,
    chains: &[TrigramChain],
    threshold: usize,
    bitset: BitSet,
    highlights: Vec<(DocId, usize, usize)>,
    coverage: Vec<(DocId, f32)>,
) -> (BitSet, Vec<(DocId, usize, usize)>, Vec<(DocId, f32)>) {
    if ctx.posmap.is_none() || ctx.termtexts.is_none() {
        return (bitset, highlights, coverage);
    }

    // In relaxed mode the query arrives already separator-stripped, so the window
    // has to be stripped too or the two are not in the same space.
    let strip = !strict_separators;
    let mut needle_s = String::with_capacity(query.len());
    for c in query.chars() {
        if strip && !is_content_char(c) { continue; }
        for lc in c.to_lowercase() { needle_s.push(lc); }
    }
    let needle: Vec<u8> = needle_s.into_bytes();
    // Enough slack for the match to start or end outside the chain's own tokens.
    let margin = 1 + (distance as u32);

    let mut kept: HashSet<DocId> = HashSet::new();
    let mut window = String::new();
    let mut buf: Vec<u32> = Vec::new();

    let diag = std::env::var("V3_DIAG_FUZZY").is_ok();
    let mut n_cand = 0usize;
    let mut n_no_window = 0usize;
    let mut n_rejected = 0usize;

    for chain in chains {
        if chain.trigram_indices.len() < threshold { continue; }
        if kept.contains(&chain.doc_id) { continue; }
        n_cand += 1;
        if !rebuild_window(ctx, chain.doc_id, chain.first_pos, chain.last_pos,
                           margin, strip, &mut window) {
            n_no_window += 1;
            if diag && n_no_window <= 3 {
                eprintln!("[fz] doc={} pos={}..{} NO WINDOW",
                    chain.doc_id, chain.first_pos, chain.last_pos);
            }
            continue;
        }
        if within_edit_distance(&needle, window.as_bytes(), distance as usize, &mut buf) {
            kept.insert(chain.doc_id);
        } else {
            n_rejected += 1;
            if diag && n_rejected <= 5 {
                eprintln!("[fz] doc={} pos={}..{} REJECT needle={:?} window={:?}",
                    chain.doc_id, chain.first_pos, chain.last_pos,
                    String::from_utf8_lossy(&needle),
                    &window[..window.len().min(80)]);
            }
        }
    }
    if diag {
        eprintln!("[fz] query={query:?} d={distance} strip={strip} cand={n_cand} \
kept={} no_window={n_no_window} rejected={n_rejected}", kept.len());
    }

    let mut out_bitset = BitSet::with_max_value(max_doc);
    for &doc in &kept { out_bitset.insert(doc); }
    let out_hl = highlights.into_iter().filter(|(d, _, _)| kept.contains(d)).collect();
    let out_cov = coverage.into_iter().filter(|(d, _)| kept.contains(d)).collect();
    (out_bitset, out_hl, out_cov)
}

// The fuzzy explain block (FuzzyExplain / TrigramExplain /
// resolve_trigrams_v3_explained) lived here. It had no caller anywhere in the
// workspace, and it still described the PREVIOUS pipeline — sliding-window
// counting over `max_window` — while production had moved to
// resolve_all_trigrams / build_trigram_chains / filter_by_chain_threshold.
// A diagnostic that explains code which no longer runs is worse than none: it
// costs a session to whoever trusts it. Removed. When the fuzzy pipeline is
// reworked, instrument the briques themselves.

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suffix_fst::builder_v3::SuffixFstBuilderV3;
    use crate::suffix_fst::collector_v3::SfxCollectorV3;
    use crate::suffix_fst::file_v3::SfxFileWriterV3;
    use crate::suffix_fst::sfxpost_v2::SfxPostReaderV2;
    use crate::query::posting_resolver::PostingEntry;

    struct MockResolver(SfxPostReaderV2);

    impl MockResolver {
        fn new(data: &[u8]) -> Self {
            Self(SfxPostReaderV2::open_slice(data).unwrap())
        }
    }

    impl PostingResolver for MockResolver {
        fn resolve(&self, ordinal: u64) -> Vec<PostingEntry> {
            self.0.entries(ordinal as u32).into_iter().map(|e| PostingEntry {
                doc_id: e.doc_id,
                position: e.token_index,
                byte_from: e.byte_from,
                byte_to: e.byte_to,
            }).collect()
        }
    }

    /// Returns (sfx_bytes, sfxpost_bytes, word_sfxpost_bytes, posmap_bytes, bytemap_bytes)
    fn build_index(texts: &[&str]) -> (Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>) {
        let mut collector = SfxCollectorV3::new();
        for text in texts {
            collector.begin_doc();
            collector.add_value(text);
            collector.end_doc();
        }
        let data = collector.into_data();

        let mut builder = SuffixFstBuilderV3::with_min_suffix_len(1);
        for &intern_ord in &data.sorted_indices {
            let meta = &data.token_meta[intern_ord as usize];
            if meta.is_word_stripped { continue; }
            let text = &data.token_texts[intern_ord as usize];
            let content_ord = data.intern_to_final[intern_ord as usize];
            builder.add_token(text, content_ord as u64, meta.own_len, meta.sep_len,
                meta.overlap_len, meta.is_word_start);
        }
        for ws in &data.word_stripped {
            let final_ord = data.intern_to_final[ws.first_intern_ord as usize];
            builder.add_word_stripped(
                &ws.word_content, &ws.content_overlap,
                final_ord as u64, ws.first_own_len, ws.last_sep_len, ws.is_word_start,
            );
        }
        let (fst_data, parent_data) = builder.build().unwrap();

        let num_terms = data.num_content_ords;
        let mut post_writer = crate::suffix_fst::sfxpost_v2::SfxPostWriterV2::new(num_terms);
        for (content_ord, postings) in data.content_postings.iter().enumerate() {
            for &(doc_id, ti, bf, bt) in postings {
                post_writer.add_entry(content_ord as u32, doc_id, ti, bf, bt);
            }
        }
        let sfxpost = post_writer.finish();
        let word_sfxpost = data.word_sfxpost;

        // Build derived indexes (posmap, bytemap)
        let derived = crate::suffix_fst::index_registry::build_derived_indexes_v3(
            &data.tokens, Some(&sfxpost), Some(&data.own_lens),
        );
        let posmap_bytes = derived.iter()
            .find(|(ext, _)| ext == "posmap")
            .map(|(_, d)| d.clone()).unwrap_or_default();
        let bytemap_bytes = derived.iter()
            .find(|(ext, _)| ext == "bytemap")
            .map(|(_, d)| d.clone()).unwrap_or_default();

        let writer = SfxFileWriterV3::new(fst_data, parent_data, data.num_docs);
        (writer.to_bytes(), sfxpost, word_sfxpost, posmap_bytes, bytemap_bytes)
    }

    // ── find_literal_v3 ──

    #[test]
    fn test_find_literal_single_token() {
        let (sfx, post, _word_sfxpost, _posmap, _bytemap) = build_index(&["mutex_lock", "hello_world"]);
        let reader = SfxFileReaderV3::open(&sfx).unwrap();
        let resolver = MockResolver::new(&post);

        // "tex" is within a single token
        let ctx = BriquesContext {
            reader: &reader, resolver: &resolver, filter_docs: None,
            debug: false,
            trace_id: None,
            posmap: None, bytemap: None, word_sfxpost: None, sibling_v3: None, termtexts: None,
        };
        let matches = find_literal_v3(&ctx, "tex", false, true);
        assert!(!matches.is_empty());
        assert_eq!(matches[0].doc_id, 0);
    }

    #[test]
    fn test_find_literal_cross_token() {
        let (sfx, post, _word_sfxpost, _posmap, _bytemap) = build_index(&["mutex_lock"]);
        let reader = SfxFileReaderV3::open(&sfx).unwrap();
        let resolver = MockResolver::new(&post);

        // "mutex_lock" spans two tokens
        let ctx = BriquesContext {
            reader: &reader, resolver: &resolver, filter_docs: None,
            debug: false,
            trace_id: None,
            posmap: None, bytemap: None, word_sfxpost: None, sibling_v3: None, termtexts: None,
        };
        let matches = find_literal_v3(&ctx, "mutex_lock", false, true);
        assert!(!matches.is_empty());
        assert_eq!(matches[0].doc_id, 0);
        assert!(matches[0].span >= 2);
    }

    #[test]
    fn test_find_literal_sep_skip() {
        let (sfx, post, word_sfxpost, posmap_bytes, bytemap_bytes) = build_index(&["mutex_lock"]);
        let reader = SfxFileReaderV3::open(&sfx).unwrap();
        let resolver = MockResolver::new(&post);
        let pm = crate::suffix_fst::posmap::PosMapReader::open(&posmap_bytes);
        let bm = crate::suffix_fst::bytemap::ByteBitmapReader::open(&bytemap_bytes);
        let wsp = crate::suffix_fst::word_sfxpost::WordSfxPostReader::open(&word_sfxpost);

        // "mutexlock" (no sep) with strict_sep=false
        let ctx = BriquesContext {
            reader: &reader, resolver: &resolver, filter_docs: None,
            debug: false,
            trace_id: None,
            posmap: pm, bytemap: bm, word_sfxpost: wsp, sibling_v3: None, termtexts: None,
        };
        let matches = find_literal_v3(&ctx, "mutexlock", false, false);
        assert!(!matches.is_empty(), "sep-skip should find match");
    }

    #[test]
    fn test_find_literal_anchor_start() {
        let (sfx, post, _word_sfxpost, _posmap, _bytemap) = build_index(&["mutex_lock"]);
        let reader = SfxFileReaderV3::open(&sfx).unwrap();
        let resolver = MockResolver::new(&post);

        // "mutex" with anchor_start → should find at SI=0
        let ctx = BriquesContext {
            reader: &reader, resolver: &resolver, filter_docs: None,
            debug: false,
            trace_id: None,
            posmap: None, bytemap: None, word_sfxpost: None, sibling_v3: None, termtexts: None,
        };
        let matches = find_literal_v3(&ctx, "mutex_lo", true, true);
        assert!(!matches.is_empty());
        assert!(matches.iter().all(|m| m.sti == 0));

        // "tex" with anchor_start → NOT at SI=0
        let matches = find_literal_v3(&ctx, "tex_lo", true, true);
        assert!(matches.is_empty());
    }

    // ── find_multi_token_v3 ──

    #[test]
    fn test_multi_token_basic() {
        let (sfx, post, _word_sfxpost, _posmap, _bytemap) = build_index(&["mutex_lock_init"]);
        let reader = SfxFileReaderV3::open(&sfx).unwrap();
        let resolver = MockResolver::new(&post);

        let tokens = vec!["mutex_lo", "lock_in", "init"];
        let ctx = BriquesContext {
            reader: &reader, resolver: &resolver, filter_docs: None,
            debug: false,
            trace_id: None,
            posmap: None, bytemap: None, word_sfxpost: None, sibling_v3: None, termtexts: None,
        };
        let matches = find_multi_token_v3(&ctx, &tokens, false, false, true);
        assert!(!matches.is_empty(), "multi-token should match");
        assert_eq!(matches[0].span, 3);
    }

    #[test]
    fn test_multi_token_no_match() {
        let (sfx, post, _word_sfxpost, _posmap, _bytemap) = build_index(&["mutex_lock"]);
        let reader = SfxFileReaderV3::open(&sfx).unwrap();
        let resolver = MockResolver::new(&post);

        // "hello" + "world" not in "mutex_lock"
        let tokens = vec!["hello", "world"];
        let ctx = BriquesContext {
            reader: &reader, resolver: &resolver, filter_docs: None,
            debug: false,
            trace_id: None,
            posmap: None, bytemap: None, word_sfxpost: None, sibling_v3: None, termtexts: None,
        };
        let matches = find_multi_token_v3(&ctx, &tokens, false, false, true);
        assert!(matches.is_empty());
    }

    // ── generate_trigrams ──

    #[test]
    fn test_trigrams_basic() {
        let (grams, _positions, n) = generate_trigrams("mutex_lock", 1);
        assert_eq!(n, 3);
        assert!(grams.contains(&"mut".to_string()));
        assert!(grams.contains(&"x_l".to_string()));
        assert!(grams.contains(&"ock".to_string()));
    }

    #[test]
    fn test_trigrams_short_query() {
        let (grams, _positions, n) = generate_trigrams("abc", 1);
        // len=3 <= 3*(1+1)=6 → bigrams
        assert_eq!(n, 2);
        assert_eq!(grams, vec!["ab", "bc"]);
    }

    #[test]
    fn test_trigrams_with_seps() {
        // Query keeps seps — no concat_query
        let (grams, positions, n) = generate_trigrams("mutex_lock", 1);
        assert_eq!(n, 3);
        // Sep byte "_" is part of the trigrams
        assert!(grams.contains(&"x_l".to_string()), "sep bytes should be in trigrams");
        assert!(grams.contains(&"ex_".to_string()));
        assert!(grams.contains(&"_lo".to_string()));
        // Positions are sequential
        assert_eq!(positions[0], 0); // "mut" at byte 0
        assert_eq!(positions[1], 1); // "ute" at byte 1
    }

    // ── resolve_trigrams_v3 ──

    fn make_ctx<'a>(
        reader: &'a SfxFileReaderV3,
        resolver: &'a dyn PostingResolver,
        wsp: &'a [u8],
        pm: &'a [u8],
        bm: &'a [u8],
    ) -> BriquesContext<'a> {
        use crate::suffix_fst::word_sfxpost::WordSfxPostReader;
        use crate::suffix_fst::posmap::PosMapReader;
        use crate::suffix_fst::bytemap::ByteBitmapReader;
        BriquesContext {
            reader,
            resolver,
            filter_docs: None,
            debug: false,
            trace_id: None,
            posmap: PosMapReader::open(pm),
            bytemap: ByteBitmapReader::open(bm),
            word_sfxpost: WordSfxPostReader::open(wsp),
            sibling_v3: None,
            termtexts: None,
        }
    }

    #[test]
    fn test_fuzzy_basic() {
        let (sfx, post, wsp, pm, bm) = build_index(&["mutex_lock", "hello_world"]);
        let reader = SfxFileReaderV3::open(&sfx).unwrap();
        let resolver = MockResolver::new(&post);
        let ctx = make_ctx(&reader, &resolver, &wsp, &pm, &bm);

        // "mutex_lck" d=1 (missing 'o') should find "mutex_lock"
        let (bitset, highlights, _) =
            resolve_trigrams_v3(&ctx, "mutex_lck", 1, true, 2);

        assert!(bitset.contains(0), "doc 0 should match fuzzy");
        assert!(!highlights.is_empty());
    }

    #[test]
    fn test_fuzzy_no_concat_query() {
        let (sfx, post, wsp, pm, bm) = build_index(&["mutex_lock"]);
        let reader = SfxFileReaderV3::open(&sfx).unwrap();
        let resolver = MockResolver::new(&post);
        let ctx = make_ctx(&reader, &resolver, &wsp, &pm, &bm);

        // Query with sep "_" kept as-is (NOT stripped by concat_query)
        // Trigrams include "x_l" which is in the FST thanks to overlap
        let (bitset, _, _) =
            resolve_trigrams_v3(&ctx, "mutex_lock", 0, true, 1);

        assert!(bitset.contains(0), "exact query should match via trigrams");
    }

    #[test]
    fn test_fuzzy_sep_skip() {
        let (sfx, post, wsp, pm, bm) = build_index(&["mutex_lock"]);
        let reader = SfxFileReaderV3::open(&sfx).unwrap();
        let resolver = MockResolver::new(&post);
        let ctx = make_ctx(&reader, &resolver, &wsp, &pm, &bm);

        // "mutexlock" (no seps) d=1 strict_sep=false
        // Trigrams "exl" and "xlo" found in stripped partition
        let (bitset, _, _) =
            resolve_trigrams_v3(&ctx, "mutexlock", 1, false, 1);

        assert!(bitset.contains(0), "fuzzy with sep-skip should find match");
    }

    #[test]
    fn test_fuzzy_no_match() {
        let (sfx, post, wsp, pm, bm) = build_index(&["mutex_lock"]);
        let reader = SfxFileReaderV3::open(&sfx).unwrap();
        let resolver = MockResolver::new(&post);
        let ctx = make_ctx(&reader, &resolver, &wsp, &pm, &bm);

        // "zzzzzzzzz" should not match anything
        let (bitset, _, _) =
            resolve_trigrams_v3(&ctx, "zzzzzzzzz", 1, true, 1);

        assert!(!bitset.contains(0));
    }

    // ── TableFunction FN investigation ──

    /// Reproduces the ground truth FN: "TableFunction" relaxed should match
    /// doc containing "standalone table functions" (binder_error.test).
    #[test]
    fn test_tablefunction_relaxed_real_docs() {
        // Load real files from the bench repo
        let fn_path = "/tmp/rag3db-bench/test/test_files/exceptions/binder/binder_error.test";
        let tp_path = "/tmp/rag3db-bench/extension/delta/src/function/delta_scan.cpp";

        let fn_content = match std::fs::read_to_string(fn_path) {
            Ok(s) => s,
            Err(_) => { eprintln!("skip: clone rag3db to /tmp/rag3db-bench"); return; }
        };
        let tp_content = std::fs::read_to_string(tp_path).unwrap();
        let neg_content = "hello world this has nothing to do with anything";

        let texts: Vec<&str> = vec![&fn_content, &tp_content, neg_content];
        let (sfx, post, word_sfxpost, posmap_bytes, bytemap_bytes) = build_index(&texts);
        let reader = SfxFileReaderV3::open(&sfx).unwrap();
        let resolver = MockResolver::new(&post);
        let pm = crate::suffix_fst::posmap::PosMapReader::open(&posmap_bytes);
        let bm = crate::suffix_fst::bytemap::ByteBitmapReader::open(&bytemap_bytes);
        let wsp = crate::suffix_fst::word_sfxpost::WordSfxPostReader::open(&word_sfxpost);

        // --- Search ---
        let ctx = BriquesContext {
            reader: &reader, resolver: &resolver, filter_docs: None,
            debug: false,
            trace_id: None,
            posmap: pm, bytemap: bm, word_sfxpost: wsp, sibling_v3: None, termtexts: None,
        };
        let matches = find_literal_v3(&ctx, "tablefunction", false, false);

        let mut matched_docs: std::collections::HashSet<u32> = std::collections::HashSet::new();
        for m in &matches {
            matched_docs.insert(m.doc_id);
        }

        // doc 1 (delta_scan.cpp) should match (has literal "TableFunction")
        assert!(matched_docs.contains(&1), "delta_scan.cpp should match 'tablefunction'");

        // doc 0 (binder_error.test) should also match ("standalone table functions")
        // This is the FN we're investigating
        assert!(matched_docs.contains(&0),
            "binder_error.test should match 'tablefunction' via 'table' + 'functions' word chain");

        // doc 2 should NOT match
        assert!(!matched_docs.contains(&2), "unrelated doc should not match");
    }

    // ── DAG parity tests ──

    use crate::suffix_fst::briques::dag_builder::{find_literal_v3_dag, find_literal_v3_dag_explained};

    #[test]
    fn test_dag_parity_single_token() {
        let (sfx, post, _ws, _pm, _bm) = build_index(&["mutex_lock", "hello_world"]);
        let reader = SfxFileReaderV3::open(&sfx).unwrap();
        let resolver = MockResolver::new(&post);
        let ctx = BriquesContext {
            reader: &reader, resolver: &resolver, filter_docs: None,
            debug: false, trace_id: None,
            posmap: None, bytemap: None, word_sfxpost: None, sibling_v3: None, termtexts: None,
        };

        let imperative = find_literal_v3(&ctx, "tex", false, true);
        let r = find_literal_v3_dag(&ctx, "tex", false, true);

        assert_eq!(imperative.len(), r.matches.len(),
            "DAG should produce same match count as imperative");
        for (i, (a, b)) in imperative.iter().zip(r.matches.iter()).enumerate() {
            assert_eq!(a.doc_id, b.doc_id, "match {i} doc_id mismatch");
            assert_eq!(a.position, b.position, "match {i} position mismatch");
        }
        assert!(r.dag_info.node_results.len() >= 3, "DAG should have at least 3 nodes");
    }

    #[test]
    fn test_dag_parity_cross_token() {
        let (sfx, post, _ws, _pm, _bm) = build_index(&["mutex_lock"]);
        let reader = SfxFileReaderV3::open(&sfx).unwrap();
        let resolver = MockResolver::new(&post);
        let ctx = BriquesContext {
            reader: &reader, resolver: &resolver, filter_docs: None,
            debug: false, trace_id: None,
            posmap: None, bytemap: None, word_sfxpost: None, sibling_v3: None, termtexts: None,
        };

        let imperative = find_literal_v3(&ctx, "mutex_lock", false, true);
        let r = find_literal_v3_dag(&ctx, "mutex_lock", false, true);

        assert_eq!(imperative.len(), r.matches.len());
        for (i, (a, b)) in imperative.iter().zip(r.matches.iter()).enumerate() {
            assert_eq!(a.doc_id, b.doc_id, "match {i} doc_id mismatch");
            assert_eq!(a.position, b.position, "match {i} position mismatch");
            assert_eq!(a.span, b.span, "match {i} span mismatch");
        }
    }

    #[test]
    fn test_dag_parity_relaxed() {
        let (sfx, post, word_sfxpost, posmap_bytes, bytemap_bytes) = build_index(&["mutex_lock"]);
        let reader = SfxFileReaderV3::open(&sfx).unwrap();
        let resolver = MockResolver::new(&post);
        let pm = crate::suffix_fst::posmap::PosMapReader::open(&posmap_bytes);
        let bm = crate::suffix_fst::bytemap::ByteBitmapReader::open(&bytemap_bytes);
        let wsp = crate::suffix_fst::word_sfxpost::WordSfxPostReader::open(&word_sfxpost);

        let ctx = BriquesContext {
            reader: &reader, resolver: &resolver, filter_docs: None,
            debug: false, trace_id: None,
            posmap: pm, bytemap: bm, word_sfxpost: wsp, sibling_v3: None, termtexts: None,
        };

        let imperative = find_literal_v3(&ctx, "mutexlock", false, false);
        let r = find_literal_v3_dag(&ctx, "mutexlock", false, false);

        assert_eq!(imperative.len(), r.matches.len(),
            "DAG relaxed should match imperative: imp={} dag={}", imperative.len(), r.matches.len());
        assert!(r.dag_info.node_results.len() >= 5, "relaxed DAG should have word pipeline nodes");
    }

    #[test]
    fn test_dag_explain_output() {
        let (sfx, post, _ws, _pm, _bm) = build_index(&["mutex_lock", "hello_world"]);
        let reader = SfxFileReaderV3::open(&sfx).unwrap();
        let resolver = MockResolver::new(&post);
        let ctx = BriquesContext {
            reader: &reader, resolver: &resolver, filter_docs: None,
            debug: false, trace_id: None,
            posmap: None, bytemap: None, word_sfxpost: None, sibling_v3: None, termtexts: None,
        };

        let r = find_literal_v3_dag(&ctx, "mutex", false, true);

        // Verify metrics
        let fst = r.dag_info.get("fst_candidates").expect("fst_candidates node missing");
        assert!(fst.metrics.iter().any(|(k, _)| k == "candidates"));

        let merge = r.dag_info.get("merge").expect("merge node missing");
        assert!(merge.metrics.iter().any(|(k, _)| k == "matches"));

        // Verify mermaid output
        let mermaid = r.dump_mermaid();
        assert!(mermaid.starts_with("graph TD"));
        assert!(mermaid.contains("fst_candidates"));
        assert!(mermaid.contains("merge"));
        assert!(mermaid.contains("-->"));
    }

    #[test]
    fn test_dag_explained_has_edge_data() {
        let (sfx, post, _ws, _pm, _bm) = build_index(&["mutex_lock", "hello_world"]);
        let reader = SfxFileReaderV3::open(&sfx).unwrap();
        let resolver = MockResolver::new(&post);
        let ctx = BriquesContext {
            reader: &reader, resolver: &resolver, filter_docs: None,
            debug: false, trace_id: None,
            posmap: None, bytemap: None, word_sfxpost: None, sibling_v3: None, termtexts: None,
        };

        let r = find_literal_v3_dag_explained(&ctx, "mutex", false, true);

        // Should have edge annotations
        assert!(!r.annotations.entries.is_empty(),
            "explained mode should produce edge annotations");

        // Candidates annotation should contain candidates + fst_keys
        let cand_data = r.annotations.get("fst_candidates", "candidates")
            .expect("fst_candidates.candidates annotation missing");
        assert!(cand_data.contains("\"candidates\""), "should contain candidates: {}", cand_data);
        assert!(cand_data.contains("\"fst_keys\""), "should contain fst_keys: {}", cand_data);
        assert!(cand_data.contains("\"sti\""), "should contain sti field");

        // Merge results annotation
        let merge_data = r.annotations.get("merge", "results")
            .expect("merge.results annotation missing");
        assert!(merge_data.starts_with('['), "results should be JSON array: {}", merge_data);

        // dump_edge_data should produce valid JSON-like output
        let json = r.dump_edge_data();
        assert!(json.contains("fst_candidates.candidates"));
    }

}
