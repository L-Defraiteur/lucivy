//! Regex by verification: required literals locate, the real regex decides.
//!
//! The previous regex pipeline approximated the pattern on the index — gap
//! classes, a DFA walked over ordinal paths — and on a 4 600-file panel it
//! answered 0 documents to `std::[a-z_]+_ptr` and `[A-Z][a-z]+Function`, 13 of
//! 160 to `function\s*\(`, with highlights covering the literal alone. It is
//! the "filter, do not verify" shape the contains and fuzzy pipelines lost
//! today, and it cannot hold a regex semantics the index does not encode.
//!
//! Here, as for fuzzy: a literal every match must contain is extracted with
//! `regex-syntax` (longest exact prefix set, else suffix set), its occurrences
//! come from the exact contains pipeline, the text around each is rebuilt
//! from termtexts with a back-map to source bytes, and `regex::Regex` — the
//! same engine the ground truth uses — runs on that window. A match touching
//! a cut edge of the window grows the window and retries, up to
//! `MAX_WINDOW_BYTES`; beyond that the match is reported as truncated in the
//! diagnostics, which is where a user warning belongs.

use std::collections::HashSet;

use regex_syntax::hir::literal::{Extractor, ExtractKind};

use super::composite;
use super::context::BriquesContext;
use super::profile;
use crate::DocId;

/// Longest window rebuilt around one literal hit, in content bytes per side.
const MAX_WINDOW_BYTES: u32 = 65_536;
/// First window size, per side.
const FIRST_WINDOW_BYTES: u32 = 128;
/// Shortest literal worth looking up: a one-byte literal hits everything.
const MIN_LITERAL_LEN: usize = 2;

/// What the pattern lets the index locate.
pub struct RegexPlan {
    /// Literals such that every match contains at least one of them.
    pub literals: Vec<String>,
    /// Which side they sit on: `true` = prefixes (match starts with one).
    pub prefix: bool,
}

/// Required literals of `pattern`: the longest finite set of exact prefixes,
/// else of exact suffixes. `None` when neither exists (`[a-z]+`, `.*`): the
/// index cannot locate candidates and the query must say so.
pub fn plan(pattern: &str) -> Option<RegexPlan> {
    // Parsed case-SENSITIVE on purpose: the contains pipeline is already
    // case-insensitive, and a case-insensitive HIR turns `function` into a
    // class per byte that the extractor expands into 64 variants — 64
    // contains queries for one literal (1.5 s of CPU on rag3db, measured).
    let hir = regex_syntax::ParserBuilder::new()
        .build()
        .parse(pattern)
        .ok()?;
    let pick = |kind: ExtractKind| -> Option<Vec<String>> {
        let mut ex = Extractor::new();
        ex.kind(kind).limit_class(8).limit_total(64).limit_repeat(4);
        let seq = ex.extract(&hir);
        let lits = seq.literals()?; // None = infinite
        let mut out = Vec::new();
        for l in lits {
            // Use the exact part only: an inexact literal is still a required
            // prefix (or suffix) of the match, just not the whole of it.
            let bytes = l.as_bytes();
            let s = String::from_utf8(bytes.to_vec()).ok()?.to_lowercase();
            if s.len() < MIN_LITERAL_LEN { return None; }
            out.push(s);
        }
        out.sort();
        out.dedup();
        if out.is_empty() { None } else { Some(out) }
    };
    if let Some(p) = pick(ExtractKind::Prefix) {
        return Some(RegexPlan { literals: p, prefix: true });
    }
    if let Some(s) = pick(ExtractKind::Suffix) {
        return Some(RegexPlan { literals: s, prefix: false });
    }
    None
}

/// Every match of `pattern` in the segment, as `(doc, from, to)` source byte
/// spans, found through `plan` and verified by `regex::Regex`.
pub fn regex_verified(
    ctx: &BriquesContext<'_>,
    pattern: &str,
    plan: &RegexPlan,
    re: &regex::Regex,
) -> Vec<(DocId, usize, usize)> {
    let diag = std::env::var("V3_DIAG_REGEX").is_ok();
    if ctx.posmap.is_none() || ctx.termtexts.is_none() {
        if diag { eprintln!("[rx] {pattern:?}: no posmap/termtexts, cannot verify"); }
        return Vec::new();
    }

    // Literal hits: exact contains, strict separators (the regex sees the
    // raw text). No verify_literal: the regex is the verification.
    let t = profile::Timer::start();
    let mut hits: Vec<(DocId, u32, u32)> = Vec::new(); // (doc, first_pos, last_pos)
    for lit in &plan.literals {
        for m in composite::find_literal_v3(ctx, lit, false, true) {
            hits.push((m.doc_id, m.position, m.position + m.span.saturating_sub(1)));
        }
    }
    t.stop(|c| &c.ns_fz_resolve);
    hits.sort_unstable();
    hits.dedup();
    profile::bump(|c| &c.n_fz_hits, hits.len() as u64);
    // One window per cluster of nearby hits, not per hit: `return` has
    // 36 000 hits on rag3db, mostly a few lines apart, and every window
    // rebuilds 2 × FIRST_WINDOW_BYTES of text around it. Hits within
    // CLUSTER_POSITIONS chunk positions of each other share a window; the
    // margin on each side is unchanged, so nothing is seen less.
    const CLUSTER_POSITIONS: u32 = 24;
    let mut regions: Vec<(DocId, u32, u32)> = Vec::with_capacity(hits.len());
    for &(doc, first_pos, last_pos) in &hits {
        match regions.last_mut() {
            Some((d, _, b)) if *d == doc && first_pos <= *b + CLUSTER_POSITIONS => {
                if last_pos > *b { *b = last_pos; }
            }
            _ => regions.push((doc, first_pos, last_pos)),
        }
    }
    profile::bump(|c| &c.n_fz_regions, regions.len() as u64);

    let mut spans: HashSet<(DocId, u32, u32)> = HashSet::new();
    let mut window = String::new();
    let mut back: Vec<(u32, u8)> = Vec::new();
    let mut n_windows = 0u64;
    let mut n_grown = 0u64;
    let mut n_truncated = 0u64;
    for &(doc, first_pos, last_pos) in &regions {
        let mut margin = FIRST_WINDOW_BYTES;
        loop {
            let t = profile::Timer::start();
            let built = composite::rebuild_window_mapped(
                ctx, doc, first_pos, last_pos, margin, false, &mut window, &mut back);
            t.stop(|c| &c.ns_fz_window);
            n_windows += 1;
            let Some((cut_start, cut_end)) = built else { break };
            let wlen = window.len();
            let mut touches_edge = false;
            let t = profile::Timer::start();
            let found: Vec<(usize, usize)> = re.find_iter(&window)
                .filter(|m| m.end() > m.start())
                .map(|m| (m.start(), m.end()))
                .collect();
            t.stop(|c| &c.ns_fz_dp);
            for (s, e) in &found {
                if (cut_start && *s == 0) || (cut_end && *e == wlen) { touches_edge = true; }
            }
            if touches_edge && margin < MAX_WINDOW_BYTES {
                margin *= 4;
                n_grown += 1;
                continue;
            }
            if touches_edge { n_truncated += 1; }
            for (s, e) in found {
                if (cut_start && s == 0) || (cut_end && e == wlen) { continue; }
                let (from, _) = back[s];
                let (last, len) = back[e - 1];
                spans.insert((doc, from, last + len as u32));
            }
            break;
        }
    }
    profile::bump(|c| &c.n_fz_windows, n_windows);
    profile::bump(|c| &c.n_fz_spans, spans.len() as u64);
    if diag {
        eprintln!("[rx] {pattern:?}: literals={:?} ({}) hits={} regions={} windows={n_windows} grown={n_grown} truncated={n_truncated} spans={}",
            plan.literals, if plan.prefix { "prefix" } else { "suffix" }, hits.len(), regions.len(), spans.len());
    }
    let mut out: Vec<(DocId, usize, usize)> = spans.into_iter()
        .map(|(d, f, t)| (d, f as usize, t as usize)).collect();
    out.sort_unstable();
    out
}
