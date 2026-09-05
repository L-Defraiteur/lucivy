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

/// Shortest literal worth looking up: a one-byte literal hits everything.
const MIN_LITERAL_LEN: usize = 2;

/// What the pattern lets the index locate.
pub struct RegexPlan {
    /// Literals such that every match contains at least one of them. Empty
    /// when the pattern requires none (`[0-9]{8}`): every document is then a
    /// candidate and is scanned whole.
    pub literals: Vec<String>,
    /// Which side they sit on: `true` = prefixes (match starts with one).
    pub prefix: bool,
    /// Longest possible match in bytes, when the pattern bounds it. `None`
    /// (`.*`, `[^*]*`, `+`): candidate documents are scanned whole.
    pub max_len: Option<usize>,
}

/// Required literals of `pattern`: the longest finite set of exact prefixes,
/// else of exact suffixes. `None` when neither exists (`[a-z]+`, `.*`): the
/// index cannot locate candidates and the query must say so.
pub fn plan(pattern: &str) -> Option<RegexPlan> {
    let hir_props = regex_syntax::ParserBuilder::new().build().parse(pattern).ok()?;
    let max_len = hir_props.properties().maximum_len();
    let pick_plan = |literals: Vec<String>, prefix: bool| RegexPlan { literals, prefix, max_len };
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
        return Some(pick_plan(p, true));
    }
    if let Some(s) = pick(ExtractKind::Suffix) {
        return Some(pick_plan(s, false));
    }
    // No literal: every document is a candidate. Exact, as slow as a grep.
    Some(pick_plan(Vec::new(), true))
}

/// Every match of `pattern` in the segment, as `(doc, from, to)` source byte
/// spans, found through `plan` and verified by `regex::Regex`.
///
/// Two regimes, both exact by construction:
/// - bounded pattern (`max_len = Some(n)`): every match is at most `n` bytes
///   and contains a literal hit; hits closer than `2n + 2` positions share a
///   region (a position is at least a byte: at least the hits closer than
///   `2n + 2` bytes); the window is the region plus `n + 1` raw bytes on
///   each side.
///   A match of the file that crosses a window edge would have to contain
///   a hit of ANOTHER region within `n` bytes of this one — excluded by the
///   merge — so `find_iter` on the window sees exactly what it sees on the
///   file, leftmost-first and non-overlapping included.
/// - unbounded pattern (`.*`, `[^*]*`) or no literal: the candidate
///   documents (those with a hit, or all of them) are rebuilt whole and
///   scanned once. The first version grew windows until a match stopped
///   touching an edge, which never triggers for a match that is simply
///   cut with no partial match inside the window: 469 of 6 796 comment
///   spans lost on `/\*[^*]*\*/`, 2 764 on `(?s)/\*.*?\*/`.
pub fn regex_verified(
    ctx: &BriquesContext<'_>,
    pattern: &str,
    plan: &RegexPlan,
    re: &regex::Regex,
    max_doc: DocId,
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

    let mut spans: HashSet<(DocId, u32, u32)> = HashSet::new();
    let mut window = String::new();
    let mut back: Vec<(u32, u8)> = Vec::new();
    let mut n_windows = 0u64;
    let mut n_docs_whole = 0u64;

    let scan = |window: &str, back: &[(u32, u8)], doc: DocId, cut_start: bool, cut_end: bool,
                    spans: &mut HashSet<(DocId, u32, u32)>| {
        let wlen = window.len();
        let t = profile::Timer::start();
        for m in re.find_iter(window) {
            let (s, e) = (m.start(), m.end());
            if e == s { continue; }
            // Cannot happen with proven margins or whole documents; kept as
            // the invariant it is.
            if (cut_start && s == 0) || (cut_end && e == wlen) { continue; }
            let (from, _) = back[s];
            let (last, len) = back[e - 1];
            spans.insert((doc, from, last + len as u32));
        }
        t.stop(|c| &c.ns_fz_dp);
    };

    match (plan.literals.is_empty(), plan.max_len) {
        (true, _) | (false, None) => {
            // Whole documents: all of them, or those with a hit.
            let docs: Vec<DocId> = if plan.literals.is_empty() {
                (0..max_doc).collect()
            } else {
                let mut d: Vec<DocId> = hits.iter().map(|h| h.0).collect();
                d.dedup();
                d
            };
            for doc in docs {
                let t = profile::Timer::start();
                let built = composite::rebuild_window_opts(
                    ctx, doc, 0, u32::MAX - 1, 0, false, false, 0, &mut window, &mut back);
                t.stop(|c| &c.ns_fz_window);
                n_docs_whole += 1;
                if built.is_none() { continue; }
                scan(&window, &back, doc, false, false, &mut spans);
            }
        }
        (false, Some(n)) => {
            let n = n as u32;
            // Regions in positions: a position holds at least one byte, so
            // hits within `2n + 2` bytes are within `2n + 2` positions — the
            // same bound merges a superset, the windows only grow.
            let mut regions: Vec<(DocId, u32, u32)> = Vec::with_capacity(hits.len()); // (doc, first_pos, last_pos)
            for &(doc, first_pos, last_pos) in &hits {
                match regions.last_mut() {
                    Some((d, _, lp)) if *d == doc && first_pos <= *lp + 2 * n + 2 => {
                        if last_pos > *lp { *lp = last_pos; }
                    }
                    _ => regions.push((doc, first_pos, last_pos)),
                }
            }
            profile::bump(|c| &c.n_fz_regions, regions.len() as u64);
            for &(doc, first_pos, last_pos) in &regions {
                let t = profile::Timer::start();
                let built = composite::rebuild_window_opts(
                    ctx, doc, first_pos, last_pos, n + 1, false, false, u32::MAX, &mut window, &mut back);
                t.stop(|c| &c.ns_fz_window);
                n_windows += 1;
                let Some((cut_start, cut_end)) = built else { continue };
                scan(&window, &back, doc, cut_start, cut_end, &mut spans);
            }
        }
    }
    profile::bump(|c| &c.n_fz_windows, n_windows + n_docs_whole);
    profile::bump(|c| &c.n_fz_spans, spans.len() as u64);
    if diag {
        eprintln!("[rx] {pattern:?}: literals={:?} ({}) max_len={:?} hits={} windows={n_windows} whole_docs={n_docs_whole} spans={}",
            plan.literals, if plan.prefix { "prefix" } else { "suffix" }, plan.max_len, hits.len(), spans.len());
    }
    let mut out: Vec<(DocId, usize, usize)> = spans.into_iter()
        .map(|(d, f, t)| (d, f as usize, t as usize)).collect();
    out.sort_unstable();
    out
}
