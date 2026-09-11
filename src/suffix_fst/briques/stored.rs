//! Queries on an index without positions (`positions: false`, 4.1).
//!
//! Such an index keeps, per token ordinal, the documents it occurs in and how
//! many times (`SFP6`, `WSP6`), and nothing that says where. A query is
//! answered in two steps.
//!
//! 1. **Candidates, from the index.** The FST phase runs as on any index — it
//!    reads no position: the tokens that hold the needle, and the chains of
//!    tokens that spell it across their boundaries (`fst_walk`). A chain's
//!    documents are the intersection, position by position, of the union of
//!    its alternatives' documents; a position given as a prefix
//!    (`Alts::Prefix`) constrains nothing. There is no sibling table to
//!    supplement the walk, so every head the FST knows is walked forward —
//!    the falling walk's and the FST candidates' — which is what the
//!    positional pipeline does when it cannot check a head backwards. A fuzzy
//!    query takes the documents of the same pigeonhole pieces or n-grams as
//!    the positional pipeline (`composite::fuzzy_generator`), a regex those of
//!    its required literals (`regex_verified::plan`), or every document when
//!    it has none. What comes out is a superset of the matching documents.
//! 2. **Matches, from the stored text.** Every candidate's stored values are
//!    searched with the definitions the ground truth uses
//!    (`lucivy_core/tests/test_sfx_v3_ground_truth.rs`): for a literal,
//!    `grep_spans` and `filter_boundaries` — Unicode lowercase, separators
//!    removed on both sides in relaxed mode, overlapping occurrences, a span
//!    from the first matched character to the end of the last, word
//!    boundaries read around it; for a fuzzy query, `fuzzy_spans` or
//!    `jaro_spans` on the same folded text; for a regex, `find_iter` on the
//!    value. The spans are exact — offsets within the value, as the
//!    positional pipeline reports them — a document's frequency is its number
//!    of occurrences, and a fuzzy document's tier is its best occurrence.
//!
//! A candidate whose field has no stored value is an error, never a silent
//! miss: `positions: false` requires stored text fields
//! (`SchemaConfig::validate`).

use std::collections::HashMap;
use std::sync::Arc;

use common::OwnedBytes;

use crate::query::posting_resolver::{DocFilter, PostingResolver};
use crate::schema::Field;
use crate::suffix_fst::briques::composite::{self, FuzzyGenerator};
use crate::suffix_fst::briques::fst_walk::{self, Alts, TokenChainV3};
use crate::suffix_fst::briques::jaro_winkler::{self, FuzzyMetric};
use crate::suffix_fst::briques::regex_verified::RegexPlan;
use crate::suffix_fst::briques::{fuzzy_spans, orchestrator, resolve};
use crate::suffix_fst::file_v3::SfxFileReaderV3;
use crate::suffix_fst::gmap::GmapReader;
use crate::suffix_fst::word_sfxpost::WordSfxPostReader;
use crate::tokenizer::equal_chunk::is_content_char;
use crate::{DocId, SegmentReader};

/// `(documents with their frequency, sorted by document; highlights)` — what
/// a segment's prescan returns.
pub type PrescanOutput = (Vec<(DocId, u32)>, Vec<(DocId, usize, usize)>);

/// A fuzzy prescan's output: `PrescanOutput` plus each document's tier.
pub type FuzzyPrescanOutput = (Vec<(DocId, u32)>, Vec<(DocId, usize, usize)>, Vec<(DocId, f32)>);

/// Occurrences found in one stored value: `(from, to, score)` byte spans of
/// the value; `score` is what a fuzzy tier reads (an edit distance, a
/// similarity), 0 otherwise.
type Found = Vec<(usize, usize, f32)>;

// ─── Entry points ───────────────────────────────────────────────────────────

/// The prescan of a literal query (`contains`, `term`, `startsWith`, phrase)
/// on one segment of an index without positions.
#[allow(clippy::too_many_arguments)]
pub fn contains_prescan(
    seg_reader: &SegmentReader,
    reader: &SfxFileReaderV3,
    resolver: &dyn PostingResolver,
    field: Field,
    query: &str,
    anchor_start: bool,
    exact_match: bool,
    strict_separators: bool,
) -> crate::Result<PrescanOutput> {
    let Some(q) = orchestrator::effective_query(query, strict_separators) else {
        return Ok((Vec::new(), Vec::new()));
    };
    let strip = !strict_separators;
    let (mut needle, mut scratch) = (Vec::new(), Vec::new());
    fold_into(query, strip, &mut needle, &mut scratch);
    if needle.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    let words = WordPostings::load(seg_reader, field);
    let words = words.reader();
    let mut bits = DocBits::new(seg_reader.max_doc());
    add_literal_candidates(&mut bits, reader, resolver, words.as_ref(), &q, anchor_start, strict_separators);
    let candidates = bits.into_sorted();

    let bounded = anchor_start || exact_match;
    let (mut hay, mut back) = (Vec::new(), Vec::new());
    let found = verify_stored(seg_reader, field, &candidates, |text, out| {
        fold_into(text, strip, &mut hay, &mut back);
        for_each_occurrence(&hay, &needle, |s| {
            let (from, to) = source_span(&back, s, s + needle.len());
            if !bounded || boundaries_ok(text, from, to, exact_match) {
                out.push((from, to, 0.0));
            }
        });
    })?;
    Ok(flatten(&found))
}

/// The prescan of a fuzzy query (Levenshtein or Jaro-Winkler) on one
/// segment of an index without positions. Distance 0 is the literal query,
/// as in the positional pipeline (`orchestrator::fuzzy_v3`).
#[allow(clippy::too_many_arguments)]
pub fn fuzzy_prescan(
    seg_reader: &SegmentReader,
    reader: &SfxFileReaderV3,
    resolver: &dyn PostingResolver,
    field: Field,
    query: &str,
    distance: u8,
    strict_separators: bool,
    metric: FuzzyMetric,
) -> crate::Result<FuzzyPrescanOutput> {
    let empty = || Ok((Vec::new(), Vec::new(), Vec::new()));
    if distance > 3 {
        return empty();
    }
    let Some(q) = orchestrator::effective_query(query, strict_separators) else {
        return empty();
    };
    if distance == 0 {
        let (doc_tf, highlights) = contains_prescan(
            seg_reader, reader, resolver, field, &q, false, false, strict_separators)?;
        let coverage = highlights.iter().map(|&(d, _, _)| (d, 0.0)).collect();
        return Ok((doc_tf, highlights, coverage));
    }

    // The positional pipeline's own candidate generator, decided from the
    // FST alone; a query it cannot cut into n-grams finds nothing there,
    // and finds nothing here.
    let (ngrams, _, _, generator, _) = composite::fuzzy_generator(reader, &q, distance, strict_separators);
    if ngrams.is_empty() {
        return empty();
    }
    let lower = q.to_lowercase();
    let literals: Vec<String> = match generator {
        // An occurrence within `d` edits holds one of the `d + 1` pieces intact.
        FuzzyGenerator::Pieces(pieces) => pieces.iter().map(|&(a, b)| lower[a..b].to_string()).collect(),
        // An occurrence holds at least `threshold` of the n-grams, so at
        // least one of any `keep` of them: the rarest, as the positional
        // pipeline takes.
        FuzzyGenerator::Pivot(keep) => {
            let mut ranked: Vec<(usize, &String)> = ngrams.iter()
                .map(|g| (fst_walk::fst_candidates_count_v3(reader, g, false, strict_separators), g))
                .collect();
            ranked.sort();
            ranked.into_iter().take(keep.max(1)).map(|(_, g)| g.clone()).collect()
        }
        // Holding the threshold of the n-grams means holding one of them.
        FuzzyGenerator::AllNgrams => ngrams.clone(),
    };
    let words = WordPostings::load(seg_reader, field);
    let words = words.reader();
    let mut bits = DocBits::new(seg_reader.max_doc());
    for lit in &literals {
        add_literal_candidates(&mut bits, reader, resolver, words.as_ref(), lit, false, strict_separators);
    }
    let candidates = bits.into_sorted();

    let strip = !strict_separators;
    let (mut needle, mut scratch) = (Vec::new(), Vec::new());
    fold_into(&q, strip, &mut needle, &mut scratch);
    let d = distance as usize;
    let (mut hay, mut back) = (Vec::new(), Vec::new());
    let found = verify_stored(seg_reader, field, &candidates, |text, out| {
        fold_into(text, strip, &mut hay, &mut back);
        match metric {
            FuzzyMetric::Levenshtein => {
                for (s, e, dist) in fuzzy_spans::fuzzy_spans_long(&needle, &hay, d) {
                    let (from, to) = source_span(&back, s, e);
                    out.push((from, to, dist as f32));
                }
            }
            FuzzyMetric::JaroWinkler { min_similarity } => {
                for (s, e, sim) in jaro_winkler::jaro_spans(&needle, &hay, d, min_similarity) {
                    let (from, to) = source_span(&back, s, e);
                    out.push((from, to, sim));
                }
            }
        }
    })?;

    // Tiers, as `composite::verify_candidates` computes them: the smallest
    // verified edit distance, or the best similarity on the miss-count scale.
    let coverage = found.iter().map(|(doc, occ)| {
        let tier = match metric {
            FuzzyMetric::Levenshtein => -occ.iter().map(|o| o.2).fold(f32::INFINITY, f32::min),
            FuzzyMetric::JaroWinkler { .. } => {
                let best = occ.iter().map(|o| o.2).fold(0.0f32, f32::max);
                -((1.0 - best) * 10.0)
            }
        };
        (*doc, tier)
    }).collect();
    let (doc_tf, highlights) = flatten(&found);
    Ok((doc_tf, highlights, coverage))
}

/// The prescan of a regex on one segment of an index without positions:
/// the documents of its required literals (every document when it has
/// none), each value searched by `re` — case-insensitive, as the query and
/// the ground truth build it.
pub fn regex_prescan(
    seg_reader: &SegmentReader,
    reader: &SfxFileReaderV3,
    resolver: &dyn PostingResolver,
    field: Field,
    plan: &RegexPlan,
    re: &regex::Regex,
) -> crate::Result<PrescanOutput> {
    let max_doc = seg_reader.max_doc();
    let candidates: Vec<u32> = if plan.literals.is_empty() {
        (0..max_doc).collect()
    } else {
        let words = WordPostings::load(seg_reader, field);
        let words = words.reader();
        let mut bits = DocBits::new(max_doc);
        for lit in &plan.literals {
            // Strict: the regex reads the raw text, separators included.
            add_literal_candidates(&mut bits, reader, resolver, words.as_ref(), lit, false, true);
        }
        bits.into_sorted()
    };
    let found = verify_stored(seg_reader, field, &candidates, |text, out| {
        for m in re.find_iter(text) {
            if m.start() == m.end() {
                continue;
            }
            out.push((m.start(), m.end(), 0.0));
        }
    })?;
    Ok(flatten(&found))
}

// ─── Candidates ─────────────────────────────────────────────────────────────

/// A segment's `.word_sfxpost` and `.gmap`, held for the readers built on them.
struct WordPostings {
    bytes: Option<OwnedBytes>,
    gmap: Option<OwnedBytes>,
}

impl WordPostings {
    fn load(seg_reader: &SegmentReader, field: Field) -> Self {
        let load = |ext: &str| seg_reader.sfx_index_file(ext, field).and_then(|f| f.read_bytes().ok());
        Self { bytes: load("word_sfxpost"), gmap: load("gmap") }
    }

    /// The word postings reader, asked by global id on a dictionary segment.
    fn reader(&self) -> Option<WordSfxPostReader<'_>> {
        let r = WordSfxPostReader::open(self.bytes.as_ref()?)?;
        Some(match self.gmap.as_ref().and_then(|g| GmapReader::open(g)) {
            Some(g) => r.with_gmap(g),
            None => r,
        })
    }
}

/// A set of a segment's documents, one bit each.
struct DocBits {
    words: Vec<u64>,
    max_doc: u32,
}

impl DocBits {
    fn new(max_doc: u32) -> Self {
        Self { words: vec![0u64; (max_doc as usize).div_ceil(64)], max_doc }
    }

    #[inline]
    fn insert(&mut self, doc: u32) {
        if doc < self.max_doc {
            self.words[doc as usize / 64] |= 1u64 << (doc % 64);
        }
    }

    fn into_sorted(self) -> Vec<u32> {
        let mut out = Vec::new();
        for (i, &w) in self.words.iter().enumerate() {
            let mut w = w;
            while w != 0 {
                let b = w.trailing_zeros();
                out.push(i as u32 * 64 + b);
                w &= w - 1;
            }
        }
        out
    }
}

/// The documents of a chain: its explicit positions' document lists, the
/// shortest first, each narrowing the running set by a binary search — a
/// chain's head is usually rare and its later positions common. `lists`
/// memoizes one list per alternatives list (chains built from the same
/// remainder share it: `TokenChainV3::ordinals`).
fn chain_docs(
    chain: &TokenChainV3,
    lists: &mut HashMap<*const Vec<u64>, Arc<Vec<u32>>>,
    fetch: &dyn Fn(&[u64]) -> Vec<u32>,
) -> Vec<u32> {
    let mut mine: Vec<Arc<Vec<u32>>> = Vec::with_capacity(chain.ordinals.len());
    for alts in &chain.ordinals {
        // A prefix position is tested on the text by the positional
        // resolvers; here it constrains nothing (the set stays a superset).
        let Alts::Ids(ids) = alts else { continue };
        let docs = lists.entry(Arc::as_ptr(ids)).or_insert_with(|| Arc::new(fetch(ids))).clone();
        if docs.is_empty() {
            return Vec::new();
        }
        mine.push(docs);
    }
    mine.sort_by_key(|l| l.len());
    let Some(first) = mine.first() else { return Vec::new() };
    let mut acc: Vec<u32> = first.as_ref().clone();
    for l in &mine[1..] {
        acc.retain(|d| l.binary_search(d).is_ok());
        if acc.is_empty() {
            break;
        }
    }
    acc
}

/// Sorted, deduplicated documents of a set of chunk ordinals.
fn chunk_docs(resolver: &dyn PostingResolver, ids: &[u64]) -> Vec<u32> {
    let mut out = Vec::new();
    for &o in ids {
        resolver.for_each_doc(o, &mut |d, _| out.push(d));
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// Sorted, deduplicated documents of a set of word-stripped ordinals.
fn word_docs(words: &WordSfxPostReader<'_>, ids: &[u64]) -> Vec<u32> {
    let mut out = Vec::new();
    for &o in ids {
        words.for_each_doc(o as u32, |d, _| out.push(d));
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// Add to `bits` every document that may hold the (effective) query `q`:
/// the documents of the tokens that hold it whole, and of every chain that
/// spells it — chunk chains always, word chains in relaxed mode.
fn add_literal_candidates(
    bits: &mut DocBits,
    reader: &SfxFileReaderV3,
    resolver: &dyn PostingResolver,
    words: Option<&WordSfxPostReader<'_>>,
    q: &str,
    anchor_start: bool,
    strict_separators: bool,
) {
    // Tokens holding the whole query (all partitions the mode reads).
    let singles = fst_walk::fst_candidates_v3(reader, q, anchor_start, strict_separators);
    for c in &singles {
        if c.is_word_stripped() {
            if let Some(w) = words {
                w.for_each_doc(c.raw_ordinal as u32, |d, _| bits.insert(d));
            }
        } else {
            resolver.for_each_doc(c.raw_ordinal, &mut |d, _| bits.insert(d));
        }
    }

    // A prefix position (`Alts::Prefix`) is only built on a shard
    // dictionary; here it simply constrains nothing.
    let prefix_alts = reader.memo().is_some();
    let query_len = q.to_lowercase().len();

    // Chunk chains: every head forward.
    let mut splits = fst_walk::falling_walk_chunks(reader, q);
    let chunk_singles: Vec<_> = singles.iter().filter(|c| !c.is_word_stripped()).cloned().collect();
    splits.extend(fst_walk::splits_from_fst_candidates(&chunk_singles, query_len));
    fst_walk::sort_and_dedup_splits(&mut splits);
    let mut chains = fst_walk::cross_chunk_chain_from_splits(reader, &splits, q, prefix_alts);
    if anchor_start {
        chains.retain(|c| c.first_sti == 0);
    }
    let mut lists: HashMap<*const Vec<u64>, Arc<Vec<u32>>> = HashMap::new();
    let fetch_chunks = |ids: &[u64]| chunk_docs(resolver, ids);
    for chain in &chains {
        for d in chain_docs(chain, &mut lists, &fetch_chunks) {
            bits.insert(d);
        }
    }

    // Word chains (relaxed): separators do not exist, words are spelled
    // across their own boundaries.
    if !strict_separators {
        if let Some(w) = words {
            let mut wsplits = fst_walk::falling_walk_words(reader, q);
            wsplits.extend(
                fst_walk::splits_from_fst_candidates(&singles, query_len)
                    .into_iter()
                    .filter(|s| s.parent.sep_len != 0),
            );
            fst_walk::sort_and_dedup_splits(&mut wsplits);
            let mut wchains = fst_walk::cross_word_chain_from_splits(reader, &wsplits, q, prefix_alts);
            if anchor_start {
                wchains.retain(|c| c.first_sti == 0);
            }
            let mut wlists: HashMap<*const Vec<u64>, Arc<Vec<u32>>> = HashMap::new();
            let fetch_words = |ids: &[u64]| word_docs(w, ids);
            for chain in &wchains {
                for d in chain_docs(chain, &mut wlists, &fetch_words) {
                    bits.insert(d);
                }
            }
        }
    }
}

// ─── Verification on the stored text ────────────────────────────────────────

/// `text` folded as the ground truth folds it: Unicode lowercase, and in
/// relaxed mode (`strip`) without its separators. Each byte of `out`
/// remembers the offset and byte length of the source character it came
/// from (`back`), so that a span is reported on the source even where
/// folding changes a character's length.
pub(crate) fn fold_into(text: &str, strip: bool, out: &mut Vec<u8>, back: &mut Vec<(u32, u8)>) {
    out.clear();
    back.clear();
    for (off, ch) in text.char_indices() {
        if strip && !is_content_char(ch) {
            continue;
        }
        let n = ch.len_utf8() as u8;
        if ch.is_ascii() {
            out.push(ch.to_ascii_lowercase() as u8);
            back.push((off as u32, n));
            continue;
        }
        for lc in ch.to_lowercase() {
            let mut buf = [0u8; 4];
            for &b in lc.encode_utf8(&mut buf).as_bytes() {
                out.push(b);
                back.push((off as u32, n));
            }
        }
    }
}

/// The source span of the folded bytes `[s, e)`: from the start of the
/// character of `s` to the end of the character of `e - 1`.
#[inline]
fn source_span(back: &[(u32, u8)], s: usize, e: usize) -> (usize, usize) {
    let (last, n) = back[e - 1];
    (back[s].0 as usize, last as usize + n as usize)
}

/// Every start offset of `needle` in `hay`, overlapping ones included — the
/// ground truth's `find_all`.
pub(crate) fn for_each_occurrence(hay: &[u8], needle: &[u8], mut f: impl FnMut(usize)) {
    if needle.is_empty() || hay.len() < needle.len() {
        return;
    }
    let first = needle[0];
    let last_start = hay.len() - needle.len();
    let mut i = 0;
    while i <= last_start {
        match hay[i..=last_start].iter().position(|&b| b == first) {
            None => return,
            Some(p) => {
                let s = i + p;
                if &hay[s..s + needle.len()] == needle {
                    f(s);
                }
                i = s + 1;
            }
        }
    }
}

/// Word boundaries of a span in its source value — the ground truth's
/// `filter_boundaries`: it starts a word when the character before it is a
/// separator or the value starts there; for `exact`, it also ends one.
fn boundaries_ok(text: &str, from: usize, to: usize, exact: bool) -> bool {
    let before_ok = from == 0
        || text.get(..from).and_then(|t| t.chars().last()).is_none_or(|c| !is_content_char(c));
    if !exact {
        return before_ok;
    }
    let after_ok = to >= text.len()
        || text.get(to..).and_then(|t| t.chars().next()).is_none_or(|c| !is_content_char(c));
    before_ok && after_ok
}

/// Hand every candidate's stored values to `matcher`; return, per document
/// with at least one occurrence, its occurrences (duplicates dropped, in
/// order). Stops at the match cap (`resolve::max_matches_per_segment`) as
/// the positional resolvers do, and says so.
fn verify_stored(
    seg_reader: &SegmentReader,
    field: Field,
    candidates: &[u32],
    mut matcher: impl FnMut(&str, &mut Found),
) -> crate::Result<Vec<(DocId, Found)>> {
    let filter = seg_reader.doc_filter();
    let store = seg_reader.get_store_reader(4)
        .map_err(|e| crate::LucivyError::SystemError(format!("positions: false: open the document store: {e}")))?;
    let cap = resolve::max_matches_per_segment();
    let mut total = 0usize;
    let mut out: Vec<(DocId, Found)> = Vec::new();
    let mut found: Found = Vec::new();
    for &doc in candidates {
        if let Some(f) = filter {
            if !f.contains(doc) {
                continue;
            }
        }
        let stored: crate::LucivyDocument = store.get(doc)?;
        let mut has_value = false;
        let mut doc_found: Found = Vec::new();
        for (f, v) in stored.field_values() {
            if f != field {
                continue;
            }
            use crate::schema::document::Value;
            let Some(text) = v.as_value().as_str() else { continue };
            has_value = true;
            found.clear();
            matcher(text, &mut found);
            // Two folded starts can fall in one source character: one span.
            found.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1);
            doc_found.extend_from_slice(&found);
        }
        if !has_value {
            return Err(crate::LucivyError::SystemError(format!(
                "positions: false: document {doc} is a candidate for field {field:?} but has no stored value \
                 there — an index without positions verifies on the stored text")));
        }
        if !doc_found.is_empty() {
            total += doc_found.len();
            out.push((doc, doc_found));
            if total >= cap {
                resolve::note_truncated(total);
                break;
            }
        }
    }
    Ok(out)
}

/// `(doc, tf)` sorted by document and the highlights, from per-document
/// occurrences.
fn flatten(found: &[(DocId, Found)]) -> PrescanOutput {
    let mut doc_tf = Vec::with_capacity(found.len());
    let mut highlights = Vec::new();
    for (doc, occ) in found {
        doc_tf.push((*doc, occ.len() as u32));
        for &(from, to, _) in occ {
            highlights.push((*doc, from, to));
        }
    }
    (doc_tf, highlights)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spans(text: &str, needle: &str, strict: bool, anchor: bool, exact: bool) -> Vec<(usize, usize)> {
        let (mut n, mut nb, mut h, mut hb) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        fold_into(needle, !strict, &mut n, &mut nb);
        fold_into(text, !strict, &mut h, &mut hb);
        let mut out = Vec::new();
        for_each_occurrence(&h, &n, |s| {
            let (from, to) = source_span(&hb, s, s + n.len());
            if !(anchor || exact) || boundaries_ok(text, from, to, exact) {
                out.push((from, to));
            }
        });
        out.dedup();
        out
    }

    #[test]
    fn the_predicate_is_the_ground_truths() {
        // Overlapping occurrences, case folded.
        assert_eq!(spans("aaA", "aa", true, false, false), vec![(0, 2), (1, 3)]);
        // Strict keeps the separator; relaxed drops it on both sides, and
        // the span runs from the first matched char to the end of the last.
        assert_eq!(spans("spin_lock spinlock spin lock", "spin_lock", true, false, false), vec![(0, 9)]);
        assert_eq!(spans("spin_lock spinlock spin lock", "spin_lock", false, false, false),
            vec![(0, 9), (10, 18), (19, 28)]);
        // Folding that changes the length reports source offsets.
        assert_eq!(spans("DÉJÀ vu", "déjà", true, false, false), vec![(0, 6)]);
        // Word start and whole word read the characters around the span.
        assert_eq!(spans("unlock lock clock", "lock", true, true, false), vec![(7, 11)]);
        assert_eq!(spans("mutex mut mutable", "mut", true, true, true), vec![(6, 9)]);
        assert_eq!(spans("lock", "lock", true, true, true), vec![(0, 4)]);
        // A needle that is only separators in relaxed mode folds to nothing.
        let (mut n, mut nb) = (Vec::new(), Vec::new());
        fold_into("_ -", true, &mut n, &mut nb);
        assert!(n.is_empty());
    }

    #[test]
    fn doc_bits_round_trip() {
        let mut b = DocBits::new(200);
        for d in [0u32, 63, 64, 65, 199, 7, 64, 250] {
            b.insert(d);
        }
        assert_eq!(b.into_sorted(), vec![0, 7, 63, 64, 65, 199]);
    }
}
