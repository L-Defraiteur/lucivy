//! Query orchestrators for SFX v3.
//!
//! Thin wrappers that validate input and route to the correct briques.
//! Each function is the public entry point for one query type.
//!
//! - `contains_v3`: exact substring search (single + cross-token)
//! - `fuzzy_v3`: fuzzy substring search via trigram pigeonhole
//! - regex: see `regex_verified` (required literals + regex on rebuilt windows)

use crate::tokenizer::equal_chunk::is_content_char;

use common::BitSet;

use crate::DocId;

use super::context::BriquesContext;
use super::composite;
use super::profile;
use super::resolve::MatchV3;

/// Maximum query length in bytes. Queries longer than this are rejected.
const MAX_QUERY_LEN: usize = 2048;

// ─── contains_v3 ──────────────────────────────────────────────────────────

/// Exact substring search (d=0).
///
/// Returns matches sorted by (doc_id, position).
/// Extend matches whose tail was found through a word's content overlap.
///
/// A 0x02 key carries the first two content bytes of the next word; a match
/// consuming them ends, in the text, inside that next word — after whatever
/// separators sit between. The resolver cannot know where, so it reports the
/// excess in `overlap_overflow` and stops `byte_to` at the word's content end.
/// Here the next content chunk is found through posmap/bytemap and the end is
/// placed at its `byte_from + excess`.
///
/// Without posmap the span is left clamped: short, never wrong.
fn place_overlap_overflow(ctx: &BriquesContext<'_>, matches: &mut [MatchV3]) {
    const CONTENT_RANGES: &[(u8, u8)] = &[
        (b'0', b'9'), (b'A', b'Z'), (b'a', b'z'), (0x80, 0xFF),
    ];
    let (Some(pm), Some(bm)) = (ctx.posmap.as_ref(), ctx.bytemap.as_ref()) else { return };
    for m in matches.iter_mut() {
        if m.overlap_overflow == 0 { continue; }
        let mut p = m.position + m.span;
        // Skip pure-separator chunks; stop at the first with content.
        let next = loop {
            let Some(ord) = pm.ordinal_at(m.doc_id, p) else { break None };
            if bm.bytes_in_ranges(ord, CONTENT_RANGES) { break Some(ord as u64); }
            p += 1;
        };
        let Some(ord) = next else { continue };
        if let Some(e) = ctx.resolver.resolve_doc(ord, m.doc_id)
            .into_iter().find(|e| e.position == p)
        {
            m.byte_to = e.byte_from + m.overlap_overflow as u32;
            m.overlap_overflow = 0;
        }
    }
}

pub fn contains_v3(
    ctx: &BriquesContext<'_>,
    query: &str,
    anchor_start: bool,
    exact_match: bool,
    strict_separators: bool,
) -> Vec<MatchV3> {
    if query.is_empty() || query.len() > MAX_QUERY_LEN {
        return Vec::new();
    }

    let effective_query;
    let query_ref = if !strict_separators {
        effective_query = query.chars().filter(|c| is_content_char(*c)).collect::<String>();
        if effective_query.is_empty() {
            return Vec::new();
        }
        effective_query.as_str()
    } else {
        query
    };

    let mut matches = composite::find_literal_v3(ctx, query_ref, anchor_start, strict_separators);
    place_overlap_overflow(ctx, &mut matches);

    // Content length of the query, in BYTES — it is compared against byte spans
    // (`byte_to - byte_from`) below. Counting chars here silently broke every
    // non-ASCII query and every strict query containing a separator.
    let query_content_len = query_ref
        .chars()
        .filter(|c| is_content_char(*c))
        .map(|c| c.len_utf8() as u32)
        .sum::<u32>();
    // The old `content_len` retain lived here. It compensated for `byte_to` meaning
    // "end of the containing token": a single-token match whose query ran past the
    // token content produced a span shorter than the query and got dropped, even
    // when the match was real. Now that `byte_to` measures the match itself, the
    // condition is tautological — ordinals in 0x00/0x01 are extended, so the FST
    // key text IS the text present at every posting, and a prefix match proves the
    // occurrence. Nothing left to filter.

    if std::env::var("V3_DIAG_LITERAL").ok().as_deref() == Some(query_ref) {
        let only_byte: Option<u32> = std::env::var("V3_DIAG_BYTE").ok().and_then(|v| v.parse().ok());
        let text = |o: u64| ctx.termtexts.as_ref().and_then(|t| t.text(o as u32)).unwrap_or("?").to_string();
        for m in &matches {
            if only_byte.is_some_and(|b| b != m.byte_from) { continue; }
            let chunk_n = ctx.resolver.resolve(m.ordinal).len();
            let word_n = ctx.word_sfxpost.as_ref().map(|w| w.entries(m.ordinal as u32).len()).unwrap_or(0);
            let ws = ctx.termtexts.as_ref().and_then(|t| t.meta(m.ordinal as u32)).map(|mm| mm.is_word_stripped);
            eprintln!("[match] doc={} pos={} span={} byte=[{}..{}] token_end={} sti={} head={:?} last={:?} ovf={} head_chunk_postings={} head_word_postings={} head_is_ws={:?}",
                m.doc_id, m.position, m.span, m.byte_from, m.byte_to, m.token_end, m.sti,
                text(m.ordinal), text(m.last_ordinal), m.overlap_overflow, chunk_n, word_n, ws);
        }
    }

    // Dedup exact duplicates. Two occurrences can share a position — the
    // query twice inside one word, `INIT2INIT` for `init` — and differ only
    // by their byte offset; both are real.
    matches.sort_by_key(|m| (m.doc_id, m.position, m.byte_from));
    matches.dedup_by_key(|m| (m.doc_id, m.position, m.byte_from));

    // exact_match reads `token_end`, never the match span: `term` means "the query
    // covers the whole token", which is a statement about the container, not about
    // how many bytes matched. Deriving it from `byte_to` is what made it possible
    // to silently turn `term` into `contains`.
    let can_verify = ctx.posmap.is_some() && ctx.termtexts.is_some();
    if exact_match && !can_verify {
        matches.retain(|m| m.token_end.saturating_sub(m.byte_from) == query_content_len);
    }

    verify_literal(ctx, query_ref, strict_separators, &mut matches);
    if (anchor_start || exact_match) && can_verify {
        verify_boundaries(ctx, query_ref, strict_separators, anchor_start, exact_match, &mut matches);
    }
    matches
}

/// `anchor_start` / `exact_match`, checked on the text rather than on the
/// chain: the occurrence must begin right after a separator (or at the
/// document start), and for `exact_match` also end right before one (or at
/// the document end). Chain-level tests (`sti == 0`, `token_end`) could not
/// say this reliably — a chunk starts at SI 0 in the middle of a long word,
/// and a suffix entering through the word pipeline carried no usable
/// `token_end` — which is how `startsWith lock` came to match `unlock` and
/// `clock` (bench_sharding `t00`) and `term mut` to match `mutex`.
///
/// Relaxed mode compares with separators stripped on both sides, so
/// `rag3weaver` as a `term` covers `rag3_weaver` whole: the boundaries are
/// still read on the unstripped text.
fn verify_boundaries(
    ctx: &BriquesContext<'_>,
    query: &str,
    strict_separators: bool,
    anchor_start: bool,
    exact_match: bool,
    matches: &mut Vec<MatchV3>,
) {
    let strip = !strict_separators;
    let mut needle = String::with_capacity(query.len());
    for c in query.chars() {
        if strip && !is_content_char(c) { continue; }
        for lc in c.to_lowercase() { needle.push(lc); }
    }
    if needle.is_empty() { return; }

    let mut win = String::new();
    let mut back: Vec<(u32, u8)> = Vec::new();
    // Stripped view of the window and, per byte of it, its byte index in `win`.
    let mut view = String::new();
    let mut vmap: Vec<usize> = Vec::new();
    matches.retain(|m| {
        let last_pos = m.position + m.span.saturating_sub(1);
        let Some((cut_start, cut_end)) = composite::rebuild_window_opts(
            ctx, m.doc_id, m.position, last_pos, 1, false, true, 64, &mut win, &mut back,
        ) else {
            return true; // cannot rebuild — keep, do not invent a rejection
        };
        view.clear();
        vmap.clear();
        for (i, c) in win.char_indices() {
            if strip && !is_content_char(c) { continue; }
            let start = view.len();
            view.push(c);
            for _ in start..view.len() { vmap.push(i); }
        }
        // The occurrence this match reports, by source offset; else the first.
        let mut occ = None;
        let mut from = 0usize;
        while let Some(rel) = view[from..].find(&needle) {
            let at = from + rel;
            if back.get(vmap[at]).map(|b| b.0) == Some(m.byte_from) { occ = Some(at); break; }
            if occ.is_none() { occ = Some(at); }
            from = at + 1;
            while from < view.len() && !view.is_char_boundary(from) { from += 1; }
        }
        let Some(at) = occ else { return false };
        let start_w = vmap[at];
        let mut end_w = vmap[at + needle.len() - 1] + 1;
        while end_w < win.len() && !win.is_char_boundary(end_w) { end_w += 1; }
        let before = win[..start_w].chars().last();
        let after = win[end_w..].chars().next();
        let starts_word = before.map_or(!cut_start, |c| !is_content_char(c));
        let ends_word = after.map_or(!cut_end, |c| !is_content_char(c));
        (!anchor_start || starts_word) && (!exact_match || starts_word && ends_word)
    });
}

/// Drop matches whose text does not actually contain the query.
///
/// Chain construction is an over-approximation: it walks the FST and stitches
/// tokens together, and a stitch that should not have happened produces a match
/// no filter downstream can recognise as wrong. Two such leaks were found by
/// scaling to 50k kernel documents — a strict `TableFunction` matching
/// "migra|table function|", and a strict `__init` chaining `_` + a whole
/// intermediate token + `init` to hit "spin|_lock_init|".
///
/// Rather than chase each stitching rule, check the answer: the predicate here is
/// exactly the one the ground truth uses — does the text contain the query,
/// lowercased, separators stripped on both sides in relaxed mode. posmap and
/// termtexts make that possible without touching the docstore.
///
/// Skipped when either file is missing: the pipeline then keeps its previous
/// behaviour rather than silently dropping everything.
fn verify_literal(
    ctx: &BriquesContext<'_>,
    query: &str,
    strict_separators: bool,
    matches: &mut Vec<MatchV3>,
) {
    if matches.is_empty() || ctx.posmap.is_none() || ctx.termtexts.is_none() {
        return;
    }
    let strip = !strict_separators;
    let mut needle = String::with_capacity(query.len());
    for c in query.chars() {
        if strip && !is_content_char(c) { continue; }
        for lc in c.to_lowercase() { needle.push(lc); }
    }
    if needle.is_empty() { return; }

    // A chain can span several tokens; one token of slack on each side covers a
    // match that starts or ends inside a neighbour.
    let margin = 1u32;
    let mut window = String::new();
    let mut back: Vec<(u32, u8)> = Vec::new();
    let mut refolded = false;
    let _t = profile::Timer::start();
    matches.retain_mut(|m| {
        let last_pos = m.position + m.span.saturating_sub(1);
        let Some(src_ascii) = composite::rebuild_window_src(ctx, m.doc_id, m.position, last_pos,
                                                            margin, strip, &mut window) else {
            return true; // cannot rebuild — keep, do not invent a rejection
        };
        if !window.contains(&needle) { return false; }
        // ASCII folds byte for byte: the offsets are right, and the
        // back-map below is the expensive part — only build it when the
        // source holds a char that could fold to another length.
        if src_ascii { return true; }
        if composite::rebuild_window_opts(ctx, m.doc_id, m.position, last_pos,
                                          margin, strip, true, 64, &mut window, &mut back).is_none() {
            return true;
        }

        // Offsets were derived from suffix indexes counted on the LOWERCASED
        // key, and applied to the source bytes. They agree unless a char
        // folds to a different byte length — the Kelvin sign `K` (3 bytes)
        // to `k` (1), `İ` (2) to `i̇` (3). `'K' -> 'K'` then reported `->`
        // two bytes early (re2 unicode_casefold.h, coherence panel). The
        // back-map knows the source offset of every folded byte: when the
        // window folded to a different length, re-place the span on the
        // occurrence the match pointed at.
        let mut src_len = 0usize;
        let mut last_src = u32::MAX;
        for &(src, n) in &back {
            if src != last_src { src_len += n as usize; last_src = src; }
        }
        if src_len == window.len() { return true; }
        let mut from = 0usize;
        let mut best: Option<(u32, u32)> = None;
        while let Some(rel) = window[from..].find(&needle) {
            let at = from + rel;
            let (s_from, _) = back[at];
            let (s_last, n) = back[at + needle.len() - 1];
            let cand = (s_from, s_last + n as u32);
            best = match best {
                None => Some(cand),
                Some(b) if (cand.0 as i64 - m.byte_from as i64).abs()
                    < (b.0 as i64 - m.byte_from as i64).abs() => Some(cand),
                b => b,
            };
            from = at + 1;
            while from < window.len() && !window.is_char_boundary(from) { from += 1; }
        }
        if let Some((bf, bt)) = best {
            if bf != m.byte_from || bt != m.byte_to {
                m.byte_from = bf;
                m.byte_to = bt;
                refolded = true;
            }
        }
        true
    });
    _t.stop(|c| &c.ns_verify);
    if refolded {
        matches.sort_by_key(|m| (m.doc_id, m.position, m.byte_from));
        matches.dedup_by_key(|m| (m.doc_id, m.position, m.byte_from));
    }
}

// ─── fuzzy_v3 ─────────────────────────────────────────────────────────────

/// Fuzzy substring search via trigram pigeonhole principle.
///
/// Query is used as-is (no concat_query, no separator stripping).
/// Threshold = max(T - n*d, 1) — no boundary trigram compensation needed
/// because the overlap covers all cross-boundary trigrams.
///
/// Parameters:
/// - `distance`: Levenshtein edit distance tolerance (1-3 typical)
/// - `strict_separators`: if false, searches stripped partition too
///
/// Returns: (doc_bitset, highlights, doc_coverage)
/// - doc_bitset: which docs matched
/// - highlights: (doc_id, byte_from, byte_to) per match
/// - doc_coverage: (doc_id, score) where score = -(miss_count as f32)
pub fn fuzzy_v3(
    ctx: &BriquesContext<'_>,
    query: &str,
    distance: u8,
    strict_separators: bool,
    max_doc: DocId,
    metric: super::jaro_winkler::FuzzyMetric,
) -> (BitSet, Vec<(DocId, usize, usize)>, Vec<(DocId, f32)>) {
    // Validate input
    if query.is_empty() || query.len() > MAX_QUERY_LEN || distance > 3 {
        return (BitSet::with_max_value(max_doc), Vec::new(), Vec::new());
    }

    // For strict_sep=false: strip non-alphanum from the query
    let effective_query;
    let query_ref = if !strict_separators {
        effective_query = query.chars().filter(|c| is_content_char(*c)).collect::<String>();
        if effective_query.is_empty() {
            return (BitSet::with_max_value(max_doc), Vec::new(), Vec::new());
        }
        effective_query.as_str()
    } else {
        query
    };

    // d=0 → route to exact contains (no trigram overhead)
    if distance == 0 {
        let matches = contains_v3(ctx, query_ref, false, false, strict_separators);
        let mut bitset = BitSet::with_max_value(max_doc);
        let mut highlights = Vec::new();
        let mut coverage = Vec::new();
        for m in &matches {
            bitset.insert(m.doc_id);
            highlights.push((m.doc_id, m.byte_from as usize, m.byte_to as usize));
            coverage.push((m.doc_id, 0.0));
        }
        return (bitset, highlights, coverage);
    }

    composite::resolve_trigrams_v3(
        ctx, query_ref, distance, strict_separators, max_doc, metric,
    )
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suffix_fst::file_v3::{SfxFileReaderV3, SfxFileWriterV3};
    use crate::suffix_fst::builder_v3::SuffixFstBuilderV3;
    use crate::suffix_fst::collector_v3::SfxCollectorV3;
    use crate::suffix_fst::sfxpost_v2::SfxPostReaderV2;
    use crate::query::posting_resolver::{PostingEntry, PostingResolver};

    struct MockResolver(SfxPostReaderV2);
    impl MockResolver {
        fn new(data: &[u8]) -> Self { Self(SfxPostReaderV2::open_slice(data).unwrap()) }
    }
    impl PostingResolver for MockResolver {
        fn resolve(&self, ordinal: u64) -> Vec<PostingEntry> {
            self.0.entries(ordinal as u32).into_iter().map(|e| PostingEntry {
                doc_id: e.doc_id, position: e.token_index,
                byte_from: e.byte_from, byte_to: e.byte_to,
            }).collect()
        }
    }

    /// Build index and return all bytes needed for a BriquesContext.
    struct TestIndex {
        sfx: Vec<u8>, sfxpost: Vec<u8>, wsp: Vec<u8>, pm: Vec<u8>, bm: Vec<u8>,
    }

    fn build_index(texts: &[&str]) -> TestIndex {
        let mut collector = SfxCollectorV3::new();
        for text in texts {
            collector.begin_doc();
            collector.add_value(text);
            collector.end_doc();
        }
        let data = collector.into_data();
        let mut builder = SuffixFstBuilderV3::with_min_suffix_len(1);
        for &io in &data.sorted_indices {
            let meta = &data.token_meta[io as usize];
            if meta.is_word_stripped { continue; }
            let text = &data.token_texts[io as usize];
            let fo = data.intern_to_final[io as usize];
            builder.add_token(text, fo as u64, meta.own_len, meta.sep_len, meta.overlap_len, meta.is_word_start);
        }
        for ws in &data.word_stripped {
            let fo = data.intern_to_final[ws.first_intern_ord as usize];
            builder.add_word_stripped(&ws.word_content, &ws.content_overlap, fo as u64, ws.first_own_len, ws.last_sep_len, ws.is_word_start);
        }
        let (fst_data, parent_data) = builder.build().unwrap();
        let mut pw = crate::suffix_fst::sfxpost_v2::SfxPostWriterV2::new(data.num_content_ords);
        for (co, postings) in data.content_postings.iter().enumerate() {
            for &(d, t, bf, bt) in postings { pw.add_entry(co as u32, d, t, bf, bt); }
        }
        let sfxpost = pw.finish();
        let derived = crate::suffix_fst::index_registry::build_derived_indexes_v3(&data.tokens, Some(&sfxpost), Some(&data.own_lens));
        let pm = derived.iter().find(|(e, _)| e == "posmap").map(|(_, d)| d.clone()).unwrap_or_default();
        let bm = derived.iter().find(|(e, _)| e == "bytemap").map(|(_, d)| d.clone()).unwrap_or_default();
        let writer = SfxFileWriterV3::new(fst_data, parent_data);
        TestIndex { sfx: writer.to_bytes(), sfxpost, wsp: data.word_sfxpost, pm, bm }
    }

    // ── contains_v3 ──

    #[test]
    fn test_contains_basic() {
        let idx = build_index(&["mutex_lock", "hello_world"]);
        let reader = SfxFileReaderV3::open(&idx.sfx).unwrap();
        let resolver = MockResolver::new(&idx.sfxpost);
        let ctx = BriquesContext {
            reader: &reader, resolver: &resolver, filter_docs: None,
            debug: false,
            trace_id: None,
            posmap: None, bytemap: None, word_sfxpost: None, sibling_v3: None, termtexts: None, word_posmap: None,
        };
        let matches = contains_v3(&ctx, "mutex", false, false, true);
        assert!(!matches.is_empty());
        assert_eq!(matches[0].doc_id, 0);
    }

    #[test]
    fn test_contains_cross_token() {
        let idx = build_index(&["mutex_lock"]);
        let reader = SfxFileReaderV3::open(&idx.sfx).unwrap();
        let resolver = MockResolver::new(&idx.sfxpost);
        let ctx = BriquesContext {
            reader: &reader, resolver: &resolver, filter_docs: None,
            debug: false,
            trace_id: None,
            posmap: None, bytemap: None, word_sfxpost: None, sibling_v3: None, termtexts: None, word_posmap: None,
        };
        let matches = contains_v3(&ctx, "mutex_lock", false, false, true);
        assert!(!matches.is_empty());
    }

    #[test]
    fn test_contains_sep_skip() {
        let idx = build_index(&["mutex_lock"]);
        let reader = SfxFileReaderV3::open(&idx.sfx).unwrap();
        let resolver = MockResolver::new(&idx.sfxpost);
        let pm = crate::suffix_fst::posmap::PosMapReader::open(&idx.pm);
        let bm = crate::suffix_fst::bytemap::ByteBitmapReader::open(&idx.bm);
        let wsp = crate::suffix_fst::word_sfxpost::WordSfxPostReader::open(&idx.wsp);
        let ctx = BriquesContext {
            reader: &reader, resolver: &resolver, filter_docs: None,
            debug: false,
            trace_id: None,
            posmap: pm, bytemap: bm, word_sfxpost: wsp, sibling_v3: None, termtexts: None, word_posmap: None,
        };
        let matches = contains_v3(&ctx, "mutexlock", false, false, false);
        assert!(!matches.is_empty(), "sep-skip should work");
    }

    #[test]
    fn test_contains_strict_rejects() {
        let idx = build_index(&["mutex_lock"]);
        let reader = SfxFileReaderV3::open(&idx.sfx).unwrap();
        let resolver = MockResolver::new(&idx.sfxpost);
        let ctx = BriquesContext {
            reader: &reader, resolver: &resolver, filter_docs: None,
            debug: false,
            trace_id: None,
            posmap: None, bytemap: None, word_sfxpost: None, sibling_v3: None, termtexts: None, word_posmap: None,
        };
        let matches = contains_v3(&ctx, "mutex lock", false, false, true);
        assert!(matches.is_empty(), "strict should reject different separator");
    }

    #[test]
    fn test_contains_anchor_start() {
        let idx = build_index(&["mutex_lock"]);
        let reader = SfxFileReaderV3::open(&idx.sfx).unwrap();
        let resolver = MockResolver::new(&idx.sfxpost);
        let ctx = BriquesContext {
            reader: &reader, resolver: &resolver, filter_docs: None,
            debug: false,
            trace_id: None,
            posmap: None, bytemap: None, word_sfxpost: None, sibling_v3: None, termtexts: None, word_posmap: None,
        };
        assert!(!contains_v3(&ctx, "mutex_lo", true, false, true).is_empty());
        assert!(contains_v3(&ctx, "tex_lo", true, false, true).is_empty());
    }

    #[test]
    fn test_contains_empty_query() {
        let idx = build_index(&["mutex_lock"]);
        let reader = SfxFileReaderV3::open(&idx.sfx).unwrap();
        let resolver = MockResolver::new(&idx.sfxpost);
        let ctx = BriquesContext {
            reader: &reader, resolver: &resolver, filter_docs: None,
            debug: false,
            trace_id: None,
            posmap: None, bytemap: None, word_sfxpost: None, sibling_v3: None, termtexts: None, word_posmap: None,
        };
        assert!(contains_v3(&ctx, "", false, false, true).is_empty());
    }

    // ── fuzzy_v3 ──

    #[test]
    fn test_fuzzy_basic() {
        let idx = build_index(&["mutex_lock", "hello_world"]);
        let reader = SfxFileReaderV3::open(&idx.sfx).unwrap();
        let resolver = MockResolver::new(&idx.sfxpost);
        let ctx = BriquesContext {
            reader: &reader, resolver: &resolver, filter_docs: None,
            debug: false,
            trace_id: None,
            posmap: None, bytemap: None, word_sfxpost: None, sibling_v3: None, termtexts: None, word_posmap: None,
        };
        let (bitset, highlights, _) = fuzzy_v3(&ctx, "mutex_lck", 1, true, 2, Default::default());
        assert!(bitset.contains(0), "doc 0 should match fuzzy");
        assert!(!highlights.is_empty());
    }

    #[test]
    fn test_fuzzy_d0_routes_to_exact() {
        let idx = build_index(&["mutex_lock"]);
        let reader = SfxFileReaderV3::open(&idx.sfx).unwrap();
        let resolver = MockResolver::new(&idx.sfxpost);
        let ctx = BriquesContext {
            reader: &reader, resolver: &resolver, filter_docs: None,
            debug: false,
            trace_id: None,
            posmap: None, bytemap: None, word_sfxpost: None, sibling_v3: None, termtexts: None, word_posmap: None,
        };
        let (bitset, _, coverage) = fuzzy_v3(&ctx, "mutex_lo", 0, true, 1, Default::default());
        assert!(bitset.contains(0));
        assert!(coverage.iter().any(|&(_, score)| score == 0.0));
    }

    #[test]
    fn test_fuzzy_no_match() {
        let idx = build_index(&["mutex_lock"]);
        let reader = SfxFileReaderV3::open(&idx.sfx).unwrap();
        let resolver = MockResolver::new(&idx.sfxpost);
        let ctx = BriquesContext {
            reader: &reader, resolver: &resolver, filter_docs: None,
            debug: false,
            trace_id: None,
            posmap: None, bytemap: None, word_sfxpost: None, sibling_v3: None, termtexts: None, word_posmap: None,
        };
        let (bitset, _, _) = fuzzy_v3(&ctx, "zzzzzzzzz", 1, true, 1, Default::default());
        assert!(!bitset.contains(0));
    }

    #[test]
    fn test_fuzzy_distance_too_high() {
        let idx = build_index(&["mutex_lock"]);
        let reader = SfxFileReaderV3::open(&idx.sfx).unwrap();
        let resolver = MockResolver::new(&idx.sfxpost);
        let ctx = BriquesContext {
            reader: &reader, resolver: &resolver, filter_docs: None,
            debug: false,
            trace_id: None,
            posmap: None, bytemap: None, word_sfxpost: None, sibling_v3: None, termtexts: None, word_posmap: None,
        };
        let (bitset, _, _) = fuzzy_v3(&ctx, "mutex", 4, true, 1, Default::default());
        assert!(!bitset.contains(0));
    }
}
