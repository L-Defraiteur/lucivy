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
#[cfg(test)]
use crate::query::posting_resolver::PostingResolver;
#[cfg(test)]
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
    //
    // Strict mode splits the work by where the head ends. A head consuming
    // more than half the query is rare (few tokens end with "net_devi"), and
    // the forward walk enumerates it cheaply. A head consuming half or less is
    // the wide side — every token ending in "n" — and is not enumerated at all:
    // those occurrences are anchored on their SECOND token, which starts with
    // the narrow remainder, and the head is checked one step backwards. See
    // `second_token_anchored_v3`. The two sets are disjoint by construction.
    // 137 715 head splits for `net_device` over 50k kernel files, for 121 hits.
    // Only when the backward check can run: without posmap and termtexts the
    // forward walk must enumerate every head, as it always did.
    let half = if strict_separators && ctx.posmap.is_some() && ctx.termtexts.is_some() {
        query.len() / 2
    } else {
        0
    };
    // Relaxed mode: the word pipeline covers every occurrence on its own
    // EXCEPT matches starting deeper than WORD_SUFFIX_CAP (256 bytes) inside
    // a word — a word entry indexes that many suffixes plus a tail, so
    // `deepmark` at the bottom of a 400-byte identifier is only reachable
    // through the chunk chains (synthetic SKU corpus, 23 August: 10 of 20
    // lost without them). `.termtexts` now records the longest word of the
    // segment; when it proves no word reaches the cap, the chunk chains are
    // pure duplicate work and are skipped. Unknown (old file) → walked.
    let skip_chunk_chains = !strict_separators
        && ctx.has_word_pipeline()
        && !ctx.may_have_long_words()
        && std::env::var("V3_RELAXED_CHUNK_CHAINS").map_or(true, |v| v != "1");
    if !strict_separators && ctx.has_word_pipeline() {
        if skip_chunk_chains {
            profile::bump(|c| &c.n_relaxed_chunk_skipped, 1);
        } else {
            profile::bump(|c| &c.n_relaxed_chunk_walked, 1);
        }
    }
    if !skip_chunk_chains {
        let _t = profile::Timer::start();
        let mut chains = if half > 0 {
            let mut splits = fst_walk::falling_walk_chunks(ctx.reader, query);
            splits.retain(|s| s.query_consumed > half);
            fst_walk::cross_chunk_chain_from_splits(ctx.reader, &splits, query)
        } else {
            fst_walk::cross_chunk_chain_v3(ctx.reader, query)
        };
        _t.stop(|c| &c.ns_chunk_walk);

        // Sibling chain supplement: if sibling table is available, use it
        // for continuations. Also catches first splits missed by falling walk.
        if ctx.has_sibling_chains() {
            let _t = profile::Timer::start();
            let mut all_splits = fst_walk::falling_walk_chunks(ctx.reader, query);
            if half > 0 { all_splits.retain(|s| s.query_consumed > half); }
            // Chunk candidates only. In relaxed mode fst_candidates_v3 scans the
            // 0x02 partition too, and a word-stripped head leaking into the chunk
            // DFS produced chains like ["0ui"@1, "uint64t"] whose span started
            // on the separator before `uint64_t` — 52 extra spans on the panel,
            // surviving every other filter because this is the one place the
            // partition invariant was not enforced.
            let chunk_cands: Vec<_> = candidates.iter()
                .filter(|c| c.partition != 0x02)
                .cloned()
                .collect();
            let extra = fst_walk::splits_from_fst_candidates(&chunk_cands, query.to_lowercase().len());
            for s in extra {
                if !s.parent.is_word_start || s.parent.sep_len == 0 { continue; }
                if half > 0 && s.query_consumed <= half { continue; }
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

        let mut chains: Vec<_> = if anchor_start {
            chains.into_iter().filter(|c| c.first_sti == 0).collect()
        } else {
            chains
        };
        // Relaxed: separators do not count, so a head entered inside its own
        // separator zone consumes nothing but its overlap — the next token's
        // first bytes, which that token also matches at sti 0. Such a head is
        // redundant and reports its span from the separator (`>>;\n    uint64<<`
        // for `uint64_t`, 72 spans on the rag3db panel). Strict keeps them: a
        // query may legitimately start with a separator.
        if !strict_separators {
            if let Some(tt) = ctx.termtexts.as_ref() {
                chains.retain(|c| {
                    let ord = c.ordinals[0][0] as u32;
                    match tt.meta(ord) {
                        Some(m) => (c.first_sti as u32) < (m.own_len as u32).saturating_sub(m.sep_len as u32),
                        None => true,
                    }
                });
            }
        }
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

    // ── Short-head occurrences, anchored on the second token ──────────
    if half > 0 {
        let _t = profile::Timer::start();
        let found = second_token_anchored_v3(ctx, query, half);
        _t.stop(|c| &c.ns_chunk_anchored);
        if dbg { eprintln!("[lit]   second-token anchored matches={}", found.len()); }
        results.extend(found);
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
        let cross = match ctx.word_posmap.as_ref() {
            Some(wpm) => resolve::resolve_word_chains_v3_wordmap_grouped(
                &chains, wsp, ctx.resolver, ctx.filter_docs, pm, bm, wpm, ctx.termtexts.as_ref()),
            None => resolve::resolve_word_chains_v3(
                &chains, wsp, ctx.resolver, ctx.filter_docs, pm, bm, ctx.termtexts.as_ref()),
        };
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

/// Strict occurrences whose head consumes at most `half` bytes of the query.
///
/// For each head length `h` in `1..=half`, the token after the head starts
/// with `query[h..]` at sti 0. That remainder is matched forward with the
/// ordinary chain machinery (anchored single candidates plus sti-0 walk
/// splits), and each resulting match is then checked one position backwards:
/// the token at `position - 1` must end, within its own content, with
/// `query[..h]`. posmap names that token, termtexts gives its text and its
/// own length, one posting fetch gives its byte offset.
///
/// Each occurrence has exactly one head length, so nothing is found twice;
/// and `h <= half` is disjoint from the forward path's `consumed > half`.
fn second_token_anchored_v3(
    ctx: &BriquesContext<'_>,
    query: &str,
    half: usize,
) -> Vec<MatchV3> {
    let (Some(pm), Some(tt)) = (ctx.posmap.as_ref(), ctx.termtexts.as_ref()) else {
        return Vec::new();
    };
    let query_lower = query.to_lowercase();
    let mut out = Vec::new();

    for h in 1..=half {
        if !query_lower.is_char_boundary(h) { continue; }
        let head = &query_lower[..h];
        let rest = &query_lower[h..];
        if rest.is_empty() { break; }

        // The second token, entered at sti 0: either it holds all of `rest`
        // (a single anchored candidate) or `rest` runs past it (a walk split).
        let mut chains: Vec<TokenChainV3> = Vec::new();
        let cands = fst_walk::fst_candidates_v3(ctx.reader, rest, true, true);
        let mut single_ords: Vec<u64> = cands.iter()
            .filter(|c| c.partition != 0x02 && c.sti == 0)
            .map(|c| c.raw_ordinal).collect();
        single_ords.sort_unstable();
        single_ords.dedup();
        let n_single = single_ords.len();
        if !single_ords.is_empty() {
            chains.push(TokenChainV3 {
                ordinals: vec![std::sync::Arc::new(single_ords)],
                first_sti: 0,
                total_query_consumed: rest.len(),
                last_consumed: rest.len(),
            });
        }
        let mut splits = fst_walk::falling_walk_chunks(ctx.reader, rest);
        let n_splits_all = splits.len();
        splits.retain(|s| s.parent.sti == 0);
        chains.extend(fst_walk::cross_chunk_chain_from_splits(ctx.reader, &splits, rest));
        if ctx.debug {
            eprintln!("[anch] h={h} head={head:?} rest={rest:?} cands={} single={} splits={}/{} chains={}",
                cands.len(), n_single, splits.len(), n_splits_all, chains.len());
            for sp in splits.iter().take(4) {
                eprintln!("[anch]   split parent={:?} sti={} own={} consumed={} rem_start={} ovl_ok={}",
                    tt.text(sp.parent.raw_ordinal as u32), sp.parent.sti, sp.parent.own_len,
                    sp.query_consumed, sp.remainder_start, sp.overlap_validated);
            }
            for c in chains.iter().take(3) {
                let texts: Vec<String> = c.ordinals.iter().map(|alts| alts.iter().take(3)
                    .map(|&o| format!("{:?}", tt.text(o as u32).unwrap_or("?"))).collect::<Vec<_>>().join("|")).collect();
                eprintln!("[anch]   chain sti={} consumed={} last={} len={} toks={:?}", c.first_sti, c.total_query_consumed, c.last_consumed, c.ordinals.len(), texts);
            }
        }
        if chains.is_empty() { continue; }

        let tail_matches = resolve::resolve_chains_v3_posmap(
            &chains, ctx.resolver, ctx.filter_docs, pm);
        if ctx.debug { eprintln!("[anch]   tail_matches={}", tail_matches.len()); }

        for m in tail_matches {
            if ctx.debug {
                let prev = if m.position > 0 {
                    pm.ordinal_at(m.doc_id, m.position - 1)
                        .map(|o| format!("{:?} own={:?}", tt.text(o), tt.meta(o).map(|mm| mm.own_len)))
                } else { None };
                eprintln!("[anch]   tail doc={} pos={} span={} byte=[{}..{}] prev={:?}",
                    m.doc_id, m.position, m.span, m.byte_from, m.byte_to, prev);
            }
            if m.position == 0 { continue; }
            let prev_pos = m.position - 1;
            let Some(ord) = pm.ordinal_at(m.doc_id, prev_pos) else { continue };
            let (Some(text), Some(meta)) = (tt.text(ord), tt.meta(ord)) else { continue };
            // termtexts keeps the original case; the FST and the query are
            // lowercase. Compare in lowercase, on a char boundary of the text.
            let own = (meta.own_len as usize).min(text.len());
            let own = (0..=own).rev().find(|&b| text.is_char_boundary(b)).unwrap_or(0);
            let own_text = text[..own].to_lowercase();
            if own_text.len() < h || !own_text.ends_with(head) { continue; }
            let sti = own - h;
            let Some(p) = ctx.resolver.resolve_doc(ord as u64, m.doc_id)
                .into_iter().find(|p| p.position == prev_pos)
            else { continue };
            out.push(MatchV3 {
                doc_id: m.doc_id,
                position: prev_pos,
                span: m.span + 1,
                byte_from: p.byte_from + sti as u32,
                overlap_overflow: 0,
                byte_to: m.byte_to,
                token_end: m.token_end,
                sti: sti as u16,
                ordinal: ord as u64,
                last_ordinal: m.last_ordinal,
            });
        }
    }
    out
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
                overlap_overflow: 0,
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
    /// First chunk position of the token holding the hit.
    pub position: u32,
    /// Last chunk position: equal to `position` for a chunk hit, the word's
    /// last chunk for a hit in the word-stripped partition.
    pub last_position: u32,
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
    keep_rarest: Option<usize>,
) -> Vec<TrigramHit> {
    // One FST walk per n-gram: the selectivity pass used to walk, drop the
    // candidates, and walk again (0.5 s of 9 s on `inclde` over 50k docs).
    let mut all_cands: Vec<Vec<FstCandidateV3>> = ngrams.iter()
        .map(|gram| fst_walk::fst_candidates_v3(ctx.reader, gram, false, strict_separators))
        .collect();
    let mut selectivity: Vec<(usize, usize)> = all_cands.iter().enumerate()
        .map(|(i, c)| (i, c.len())).collect();
    selectivity.sort_by_key(|&(_, count)| count);
    if let Some(k) = keep_rarest { selectivity.truncate(k.max(1)); }

    let has_wsp = ctx.has_word_pipeline();
    let mut all_hits = Vec::new();
    let mut seen: HashSet<(DocId, u32)> = HashSet::new();

    for &(gram_idx, _) in &selectivity {
        let cands = std::mem::take(&mut all_cands[gram_idx]);
        let gram_len = ngrams[gram_idx].len() as u32;
        let chunk_matches = resolve::resolve_single_v3(&cands, ctx.resolver, None, gram_len);
        // The word partition repeats 96% of the chunk hits at the same
        // (doc, byte) — it only adds the n-grams straddling a chunk boundary
        // inside a word. Keep those; drop the echo before it is hashed,
        // sorted and regrouped downstream (10.5 M of 11 M word hits on
        // `inclde`, 50k docs).
        let mut word_matches = if has_wsp {
            resolve::resolve_single_word_v3(&cands, ctx.require_word_sfxpost(), None, gram_len)
        } else { Vec::new() };
        if !word_matches.is_empty() {
            seen.clear();
            seen.extend(chunk_matches.iter().map(|m| (m.doc_id, m.byte_from)));
            word_matches.retain(|m| !seen.contains(&(m.doc_id, m.byte_from)));
        }

        if std::env::var("V3_DIAG_FUZZY").is_ok() {
            eprintln!("[fz] gram {:?}: {} chunk hits, {} word hits: {:?}", ngrams[gram_idx],
                chunk_matches.len(), word_matches.len(),
                chunk_matches.iter().chain(word_matches.iter()).take(8)
                    .map(|m| (m.doc_id, m.position, m.byte_from)).collect::<Vec<_>>());
        }
        for m in chunk_matches.iter().chain(word_matches.iter()) {
            all_hits.push(TrigramHit {
                tri_idx: gram_idx, doc_id: m.doc_id, position: m.position,
                last_position: m.position + m.span.saturating_sub(1),
                byte_from: m.byte_from, byte_to: m.byte_to,
            });
        }
    }
    all_hits
}

/// Candidate generation by exact pieces.
///
/// Cut `query` into `d + 1` contiguous pieces; every occurrence within edit
/// distance `d` contains at least one of them unchanged, so the union of the
/// pieces' exact occurrences covers every fuzzy occurrence. Each piece is a
/// contains query (`find_literal_v3`, without its own verification: the
/// fuzzy alignment verifies anyway). Among all partitions the one with the
/// fewest FST candidates in total is used — `inc|lde`, not `i|nclde`.
/// Returns hits in the region format (piece index as `tri_idx`, piece
/// offset in the query as its position) or `None` when the query is too
/// short to cut into pieces of at least two bytes.
/// Sum of FST candidate counts of the `keep` rarest n-grams: what the pivot
/// generator would resolve. Same unit as the piece partition cost.
fn pivot_cost_estimate(
    ctx: &BriquesContext<'_>,
    ngrams: &[String],
    strict_separators: bool,
    keep: usize,
) -> usize {
    let mut counts: Vec<usize> = ngrams.iter()
        .map(|g| fst_walk::fst_candidates_v3(ctx.reader, g, false, strict_separators).len())
        .collect();
    counts.sort_unstable();
    counts.iter().take(keep.max(1)).sum()
}

/// `max_cost`: give up (return `None`) when the best partition costs more
/// than this — the caller then uses the pivot generator instead.
fn resolve_pieces(
    ctx: &BriquesContext<'_>,
    query: &str,
    distance: u8,
    strict_separators: bool,
    max_cost: Option<usize>,
) -> Option<(Vec<TrigramHit>, Vec<usize>)> {
    let lower = query.to_lowercase();
    let k = distance as usize + 1;
    const MIN_PIECE: usize = 2;
    let cuts: Vec<usize> = (1..lower.len()).filter(|&i| lower.is_char_boundary(i)).collect();
    if lower.len() < k * MIN_PIECE || cuts.len() + 1 < k { return None; }

    // Selectivity of every candidate piece [a, b): FST candidate count.
    let bounds: Vec<usize> = std::iter::once(0).chain(cuts.iter().copied()).chain(std::iter::once(lower.len())).collect();
    let mut cost: std::collections::HashMap<(usize, usize), usize> = std::collections::HashMap::new();
    let mut piece_cost = |a: usize, b: usize| -> usize {
        *cost.entry((a, b)).or_insert_with(|| {
            fst_walk::fst_candidates_v3(ctx.reader, &lower[a..b], false, strict_separators).len()
        })
    };

    // Enumerate partitions into k pieces of >= MIN_PIECE bytes; queries are
    // short (<= 64 bytes) and k <= 4, the count stays small.
    let mut best: Option<(usize, Vec<(usize, usize)>)> = None;
    fn rec(
        bounds: &[usize], start_idx: usize, left: usize, acc: &mut Vec<(usize, usize)>,
        acc_cost: usize, best: &mut Option<(usize, Vec<(usize, usize)>)>,
        piece_cost: &mut dyn FnMut(usize, usize) -> usize,
    ) {
        let a = bounds[start_idx];
        if left == 1 {
            let b = *bounds.last().unwrap();
            if b - a < MIN_PIECE { return; }
            let c = acc_cost + piece_cost(a, b);
            if best.as_ref().map_or(true, |(bc, _)| c < *bc) {
                let mut p = acc.clone(); p.push((a, b));
                *best = Some((c, p));
            }
            return;
        }
        for j in (start_idx + 1)..bounds.len() - 1 {
            let b = bounds[j];
            if b - a < MIN_PIECE { continue; }
            if bounds[bounds.len() - 1] - b < (left - 1) * MIN_PIECE { break; }
            let c = acc_cost + piece_cost(a, b);
            if best.as_ref().is_some_and(|(bc, _)| c >= *bc) { continue; }
            acc.push((a, b));
            rec(bounds, j, left - 1, acc, c, best, piece_cost);
            acc.pop();
        }
    }
    let mut acc = Vec::new();
    rec(&bounds, 0, k, &mut acc, 0, &mut best, &mut piece_cost);
    let (best_cost, pieces) = best?;
    if let Some(limit) = max_cost {
        // A piece goes through the contains pipeline — chains across
        // separators and chunks — which costs more per candidate than a
        // plain n-gram posting list. Weigh it accordingly (measured ratio
        // of resolve CPU per hit on rag3db: roughly 2).
        if best_cost * 2 > limit {
            if std::env::var("V3_DIAG_FUZZY").is_ok() {
                eprintln!("[fz] auto: pivot (pieces cost {best_cost} x2 > pivot {limit})");
            }
            return None;
        }
    }

    let mut hits = Vec::new();
    let mut positions = Vec::with_capacity(pieces.len());
    for (idx, &(a, b)) in pieces.iter().enumerate() {
        positions.push(a);
        let matches = find_literal_v3(ctx, &lower[a..b], false, strict_separators);
        for m in matches {
            hits.push(TrigramHit {
                tri_idx: idx,
                doc_id: m.doc_id,
                position: m.position,
                last_position: m.position + m.span.saturating_sub(1),
                byte_from: m.byte_from,
                byte_to: m.byte_to,
            });
        }
    }
    if std::env::var("V3_DIAG_FUZZY").is_ok() {
        eprintln!("[fz] pieces for {query:?} d={distance}: {:?} -> {} hits",
            pieces.iter().map(|&(a, b)| &lower[a..b]).collect::<Vec<_>>(), hits.len());
    }
    Some((hits, positions))
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
const MAX_SEPARATOR_SLACK: i32 = 32;

/// How many distinct chains we keep per document.

/// Group each document's n-gram hits into regions — one chain per region.
///
/// A chain is a place to look, nothing more: `verify_candidates` rebuilds the
/// text there and aligns it. So the right unit is the *region*, a run of hits
/// whose neighbours are no further apart than the query could stretch. This
/// replaces the chain-per-starting-hit walk, which produced one window per
/// hit on the same occurrence and then capped itself at eight chains per
/// document — a silent cap that dropped the ninth occurrence of `rag3weaver`
/// in a design note (280 of 1 107 occurrences missing on rag3db).
///
/// `trigram_indices` holds the DISTINCT query n-grams seen in the region, in
/// query order, so the pigeonhole threshold keeps its meaning. Very long
/// regions (repetitive text) are cut at `MAX_REGION_BYTES`; the verification
/// window's margin covers the cut.
pub fn build_trigram_chains(
    hits: &[TrigramHit],
    query_positions: &[usize],
    distance: u8,
) -> Vec<TrigramChain> {
    const MAX_REGION_BYTES: u32 = 4096;
    let query_len = query_positions.iter().copied().max().unwrap_or(0) as i64 + 3;
    // Two hits of one occurrence are at most a query length apart, plus the
    // edits and the separators relaxed mode skips.
    let max_gap = query_len + distance as i64 + MAX_SEPARATOR_SLACK as i64;

    let mut hits_by_doc: std::collections::HashMap<DocId, Vec<&TrigramHit>> =
        std::collections::HashMap::new();
    for hit in hits {
        hits_by_doc.entry(hit.doc_id).or_default().push(hit);
    }

    let mut chains = Vec::new();
    for (&doc_id, doc_hits) in &hits_by_doc {
        let mut sorted: Vec<&TrigramHit> = doc_hits.iter().copied().collect();
        sorted.sort_by_key(|h| (h.byte_from, h.tri_idx));

        let mut i = 0;
        while i < sorted.len() {
            let first = sorted[i];
            let mut last = first;
            let mut idx: Vec<usize> = vec![first.tri_idx];
            // Positions are tracked as min/max, not "first/last hit by byte":
            // a word-partition hit carries its word's FIRST chunk position, so
            // the last hit by byte can sit at an earlier position than a
            // chunk hit before it — and the window then stopped short of the
            // occurrence (`rePrun|ing` for `retrun`).
            let mut first_pos = first.position;
            let mut last_pos = first.last_position;
            let mut j = i + 1;
            while j < sorted.len() {
                let h = sorted[j];
                if h.byte_from as i64 - last.byte_from as i64 > max_gap { break; }
                if h.byte_from - first.byte_from > MAX_REGION_BYTES { break; }
                idx.push(h.tri_idx);
                first_pos = first_pos.min(h.position);
                last_pos = last_pos.max(h.last_position);
                last = h;
                j += 1;
            }
            idx.sort_unstable();
            idx.dedup();
            chains.push(TrigramChain {
                doc_id,
                trigram_indices: idx,
                byte_from: first.byte_from,
                byte_to: last.byte_to.max(first.byte_from),
                first_pos,
                last_pos: last_pos.max(first_pos),
            });
            i = j;
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
    rebuild_window_src(ctx, doc_id, first_pos, last_pos, margin, strip_separators, out).is_some()
}

/// `rebuild_window`, also reporting whether the SOURCE text of the window
/// was pure ASCII. The window itself is lowercased, and folding can turn a
/// non-ASCII char into ASCII (the Kelvin sign to `k`), so the window cannot
/// answer that question about its source.
pub(super) fn rebuild_window_src(
    ctx: &BriquesContext<'_>,
    doc_id: DocId,
    first_pos: u32,
    last_pos: u32,
    margin: u32,
    strip_separators: bool,
    out: &mut String,
) -> Option<bool> {
    let (Some(pm), Some(tt)) = (ctx.posmap.as_ref(), ctx.termtexts.as_ref()) else {
        return None;
    };
    out.clear();
    let mut ascii = true;
    let from = first_pos.saturating_sub(margin);
    let to = last_pos.saturating_add(margin);
    for pos in from..=to {
        let Some(ord) = pm.ordinal_at(doc_id, pos) else { break };
        let Some(text) = tt.text(ord) else { break };
        let own = tt.meta(ord).map(|m| m.own_len as usize).unwrap_or(text.len());
        let end = own.min(text.len());
        let src = &text[..end];
        if !src.is_ascii() { ascii = false; }
        // Lowercase to match the index: FST keys are built lowercased and the
        // query arrives lowercased, so the engine is case-insensitive for fuzzy.
        // termtexts keeps the ORIGINAL case, so comparing raw bytes here rejects
        // "Functions" for the query "functin" — a match the index did find.
        for c in src.chars() {
            if strip_separators && !is_content_char(c) { continue; }
            for lc in c.to_lowercase() { out.push(lc); }
        }
    }
    if out.is_empty() { None } else { Some(ascii) }
}

/// `rebuild_window` with a back-map: for every byte of `out`, the source
/// offset of the character it came from and that character's source length.
/// Lowercasing can change a character's byte length, so the map is per window
/// byte, not per source byte.
///
/// `margin` is in CONTENT bytes on each side of the hit region, not in
/// positions: a pure-separator run (" = ...` |\n| `") is several chunks
/// long, and a margin of two positions stopped inside it — the `in` of
/// `func … in` for `functin` sat just past the window.
///
/// Source offsets are derived, not looked up: within a value, chunk p+1
/// starts at `byte_from(p) + own_len(p)` (collector_v3: `offset += chunk_len`).
/// One posting lookup anchors the first position and one checks the last;
/// a disagreement (a value boundary, where offsets restart) falls back to a
/// lookup per position and is counted in `n_fz_window_derive_miss`. The
/// per-position lookup decoded a document's whole payload for one entry —
/// 675 M postings for 14 M used on `inclde` over 50k files.
pub(super) fn rebuild_window_mapped(
    ctx: &BriquesContext<'_>,
    doc_id: DocId,
    first_pos: u32,
    last_pos: u32,
    margin: u32,
    strip_separators: bool,
    out: &mut String,
    back: &mut Vec<(u32, u8)>,
) -> Option<(bool, bool)> {
    rebuild_window_opts(ctx, doc_id, first_pos, last_pos, margin, strip_separators, true, 64, out, back)
}

/// `lowercase`: fold the window (the fuzzy alignment compares bytes against
/// a lowercased needle); the regex path keeps the source case, its matcher
/// is case-insensitive by itself and `(?-i)` must keep working.
/// `max_extra_positions`: how far past `first_pos`/`last_pos` the margin may
/// walk, per side — a silent cap for the fuzzy (64 positions of pure
/// separators), lifted by the regex path which proves its margins.
#[allow(clippy::too_many_arguments)]
pub(super) fn rebuild_window_opts(
    ctx: &BriquesContext<'_>,
    doc_id: DocId,
    first_pos: u32,
    last_pos: u32,
    margin: u32,
    strip_separators: bool,
    lowercase: bool,
    max_extra_positions: u32,
    out: &mut String,
    back: &mut Vec<(u32, u8)>,
) -> Option<(bool, bool)> {
    let (Some(pm), Some(tt)) = (ctx.posmap.as_ref(), ctx.termtexts.as_ref()) else {
        return None;
    };
    out.clear();
    back.clear();

    // One pass per position: text, own length, content byte count.
    struct Tok<'t> { text: &'t str, own: usize, content: usize }
    let tok = |pos: u32| -> Option<Tok<'_>> {
        let ord = pm.ordinal_at(doc_id, pos)?;
        let text = tt.text(ord)?;
        let own = tt.meta(ord).map(|m| m.own_len as usize).unwrap_or(text.len()).min(text.len());
        let content = if strip_separators {
            text[..own].chars().filter(|c| is_content_char(*c)).map(|c| c.len_utf8()).sum()
        } else { own };
        Some(Tok { text, own, content })
    };

    let mut toks: std::collections::VecDeque<(u32, Tok<'_>)> = std::collections::VecDeque::new();
    for pos in first_pos..=last_pos {
        let Some(t) = tok(pos) else { break };
        toks.push_back((pos, t));
    }
    if toks.is_empty() { return None; }
    // The caller may ask past the document's end (whole-document rebuild
    // passes u32::MAX): clamp to what exists.
    let last_pos = toks.back().map(|(p, _)| *p).unwrap_or(last_pos);
    let mut have = 0usize;
    let mut from = first_pos;
    while from > 0 && have < margin as usize && first_pos - from < max_extra_positions {
        let Some(t) = tok(from - 1) else { break };
        have += t.content;
        from -= 1;
        toks.push_front((from, t));
    }
    let cut_start = from > 0;
    have = 0;
    let mut to = last_pos;
    while have < margin as usize && to - last_pos < max_extra_positions {
        let Some(t) = tok(to + 1) else { break };
        have += t.content;
        to += 1;
        toks.push_back((to, t));
    }
    let cut_end = pm.ordinal_at(doc_id, to + 1).is_some();

    // Anchor, derive, check.
    let first_ord = pm.ordinal_at(doc_id, from)?;
    let base = ctx.resolver.resolve_doc_at(first_ord as u64, doc_id, from)?.byte_from;
    profile::bump(|c| &c.n_fz_window_postings, 2);
    let mut offsets: Vec<u32> = Vec::with_capacity(toks.len());
    let mut acc = base;
    for (_, t) in &toks {
        offsets.push(acc);
        acc += t.own as u32;
    }
    let last_ord = pm.ordinal_at(doc_id, to)?;
    let derived_last = *offsets.last().unwrap();
    let actual_last = ctx.resolver.resolve_doc_at(last_ord as u64, doc_id, to)?.byte_from;
    if derived_last != actual_last {
        profile::bump(|c| &c.n_fz_window_derive_miss, 1);
        offsets.clear();
        for (pos, _) in &toks {
            let ord = pm.ordinal_at(doc_id, *pos)?;
            profile::bump(|c| &c.n_fz_window_postings, 1);
            offsets.push(ctx.resolver.resolve_doc_at(ord as u64, doc_id, *pos)?.byte_from);
        }
    }

    for ((_, t), &bf) in toks.iter().zip(offsets.iter()) {
        for (off, c) in t.text[..t.own].char_indices() {
            if strip_separators && !is_content_char(c) { continue; }
            let src = bf + off as u32;
            let len = c.len_utf8() as u8;
            if lowercase {
                for lc in c.to_lowercase() {
                    let start = out.len();
                    out.push(lc);
                    for _ in start..out.len() { back.push((src, len)); }
                }
            } else {
                out.push(c);
                for _ in 0..len { back.push((src, len)); }
            }
        }
    }
    if out.is_empty() { None } else { Some((cut_start, cut_end)) }
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
    let mut threshold = (ngrams.len() as i32 - n as i32 * distance as i32).max(1) as usize;

    // Three candidate generators, selectable for benchmarking
    // (`V3_FUZZY_MODE=ngram|pivot|pieces`, default `pieces`). All three
    // feed the same regions → windows → alignment, so their results must be
    // identical; only the cost of finding where to look differs.
    //
    // - `ngram`: every n-gram resolved in full, pigeonhole threshold on the
    //   region. 26 M hits for 217 k spans on `inclde` over 50k files: with
    //   bigrams and a threshold of 1-2 the pigeonhole filters nothing.
    // - `pivot`: only the `N - t + 1` rarest n-grams are resolved. Any
    //   occurrence holds at least `t` of the N, so it holds at least one of
    //   these; the region threshold becomes 1 (find_multi_token_v3's
    //   "pivot on the most selective" applied to n-grams).
    // - `pieces`: the query is cut into `d + 1` contiguous pieces; an
    //   occurrence within distance `d` contains at least one piece intact
    //   (classic pigeonhole). Pieces are resolved exactly by the contains
    //   pipeline, the partition chosen to minimise the posting count.
    // - `auto` (default): `pieces` or `pivot`, whichever promises fewer
    //   postings — both estimates come from the same FST candidate counts,
    //   before anything is resolved. Measured over 50k kernel files, neither
    //   generator wins alone: pieces 129 ms vs pivot 198 on `inclde`, but
    //   78 vs 59 on `spinlock` and 575 vs 480 on `__init` (a piece `in`).
    let mode = std::env::var("V3_FUZZY_MODE").unwrap_or_else(|_| "auto".into());
    let t = profile::Timer::start();
    let pivot_keep = ngrams.len() + 1 - threshold;
    let (hits, positions) = match mode.as_str() {
        "pieces" => match resolve_pieces(ctx, query, distance, strict_separators, None) {
            Some(r) => { threshold = 1; r }
            None => (resolve_all_trigrams(ctx, &ngrams, strict_separators, None), query_positions.clone()),
        },
        "pivot" => {
            threshold = 1;
            (resolve_all_trigrams(ctx, &ngrams, strict_separators, Some(pivot_keep)), query_positions.clone())
        }
        "auto" => {
            let pivot_cost = pivot_cost_estimate(ctx, &ngrams, strict_separators, pivot_keep);
            match resolve_pieces(ctx, query, distance, strict_separators, Some(pivot_cost)) {
                Some(r) => { threshold = 1; r }
                None => {
                    threshold = 1;
                    (resolve_all_trigrams(ctx, &ngrams, strict_separators, Some(pivot_keep)), query_positions.clone())
                }
            }
        }
        _ => (resolve_all_trigrams(ctx, &ngrams, strict_separators, None), query_positions.clone()),
    };
    t.stop(|c| &c.ns_fz_resolve);
    profile::bump(|c| &c.n_fz_hits, hits.len() as u64);
    let t = profile::Timer::start();
    let chains = build_trigram_chains(&hits, &positions, distance);
    t.stop(|c| &c.ns_fz_chains);
    profile::bump(|c| &c.n_fz_regions, chains.len() as u64);
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
    // Content bytes to add on each side of the hit region: a whole query
    // plus the edits, so an occurrence whose hits sit at one end is seen
    // entirely, with one byte of context for the alignment to settle.
    let margin = needle.len() as u32 + distance as u32 + 1;

    let mut kept: HashSet<DocId> = HashSet::new();
    let mut window = String::new();
    let mut back: Vec<(u32, u8)> = Vec::new();
    let mut buf: Vec<u32> = Vec::new();
    // The highlights are NOT the chain extents any more: a chain only says
    // where to look. Each window is aligned with the shared occurrence
    // definition (`fuzzy_spans`) and every occurrence it holds is mapped back
    // to source bytes. Windows of neighbouring chains overlap, so spans are
    // deduplicated per document; every chain is visited, not just the first
    // per document, or the second occurrence in a file is never reported.
    let mut spans: HashSet<(DocId, u32, u32)> = HashSet::new();
    // (first_pos, last_pos) windows already aligned for a doc.
    let mut seen_windows: HashSet<(DocId, u32, u32)> = HashSet::new();

    let diag = std::env::var("V3_DIAG_FUZZY").is_ok();
    let mut n_cand = 0usize;
    let mut n_no_window = 0usize;
    let mut n_rejected = 0usize;

    for chain in chains {
        if chain.trigram_indices.len() < threshold { continue; }
        if !seen_windows.insert((chain.doc_id, chain.first_pos, chain.last_pos)) { continue; }
        n_cand += 1;
        let t = profile::Timer::start();
        let built = rebuild_window_mapped(
            ctx, chain.doc_id, chain.first_pos, chain.last_pos, margin, strip, &mut window, &mut back);
        t.stop(|c| &c.ns_fz_window);
        let Some((cut_start, cut_end)) = built else {
            n_no_window += 1;
            if diag && n_no_window <= 3 {
                eprintln!("[fz] doc={} pos={}..{} NO WINDOW",
                    chain.doc_id, chain.first_pos, chain.last_pos);
            }
            continue;
        };
        // Cheap reject first: the full alignment only runs on windows that
        // hold something.
        let t = profile::Timer::start();
        let ok = within_edit_distance(&needle, window.as_bytes(), distance as usize, &mut buf);
        t.stop(|c| &c.ns_fz_dp);
        if !ok {
            n_rejected += 1;
            if diag && n_rejected <= 5 {
                eprintln!("[fz] doc={} pos={}..{} REJECT needle={:?} window={:?}",
                    chain.doc_id, chain.first_pos, chain.last_pos,
                    String::from_utf8_lossy(&needle),
                    &window[..window.len().min(80)]);
            }
            continue;
        }
        let found = super::fuzzy_spans::fuzzy_spans(&needle, window.as_bytes(), distance as usize);
        if found.is_empty() { continue; }
        let wlen = window.len();
        let mut any = false;
        for (s, e, _) in found {
            // An occurrence touching a cut edge of the window is only partly
            // seen here (`uint6|` at the end of a window was reported as a
            // d=1 match); the margin guarantees the window of its own region
            // sees it whole, and that one reports it.
            if (cut_start && s == 0) || (cut_end && e == wlen) { continue; }
            any = true;
            let (from, _) = back[s];
            let (last, len) = back[e - 1];
            spans.insert((chain.doc_id, from, last + len as u32));
        }
        if any { kept.insert(chain.doc_id); }
    }
    profile::bump(|c| &c.n_fz_windows, n_cand as u64);
    profile::bump(|c| &c.n_fz_rejected, n_rejected as u64);
    profile::bump(|c| &c.n_fz_spans, spans.len() as u64);
    if diag {
        eprintln!("[fz] query={query:?} d={distance} strip={strip} cand={n_cand} \
kept={} no_window={n_no_window} rejected={n_rejected} spans={}", kept.len(), spans.len());
    }

    let mut out_bitset = BitSet::with_max_value(max_doc);
    for &doc in &kept { out_bitset.insert(doc); }
    let _ = highlights;
    let mut out_hl: Vec<(DocId, usize, usize)> = spans.into_iter()
        .map(|(d, f, t)| (d, f as usize, t as usize)).collect();
    out_hl.sort_unstable();
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

        let writer = SfxFileWriterV3::new(fst_data, parent_data);
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
            posmap: None, bytemap: None, word_sfxpost: None, sibling_v3: None, termtexts: None, word_posmap: None,
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
            posmap: None, bytemap: None, word_sfxpost: None, sibling_v3: None, termtexts: None, word_posmap: None,
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
            posmap: pm, bytemap: bm, word_sfxpost: wsp, sibling_v3: None, termtexts: None, word_posmap: None,
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
            posmap: None, bytemap: None, word_sfxpost: None, sibling_v3: None, termtexts: None, word_posmap: None,
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
            posmap: None, bytemap: None, word_sfxpost: None, sibling_v3: None, termtexts: None, word_posmap: None,
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
            posmap: None, bytemap: None, word_sfxpost: None, sibling_v3: None, termtexts: None, word_posmap: None,
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
            termtexts: None, word_posmap: None,
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
            posmap: pm, bytemap: bm, word_sfxpost: wsp, sibling_v3: None, termtexts: None, word_posmap: None,
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
            posmap: None, bytemap: None, word_sfxpost: None, sibling_v3: None, termtexts: None, word_posmap: None,
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
            posmap: None, bytemap: None, word_sfxpost: None, sibling_v3: None, termtexts: None, word_posmap: None,
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
            posmap: pm, bytemap: bm, word_sfxpost: wsp, sibling_v3: None, termtexts: None, word_posmap: None,
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
            posmap: None, bytemap: None, word_sfxpost: None, sibling_v3: None, termtexts: None, word_posmap: None,
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
            posmap: None, bytemap: None, word_sfxpost: None, sibling_v3: None, termtexts: None, word_posmap: None,
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
