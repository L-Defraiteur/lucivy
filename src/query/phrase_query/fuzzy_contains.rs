//! Fuzzy contains search — dedicated pipeline.
//!
//! Concatenates the query (strips separators), generates trigrams, resolves
//! each via literal pipeline (with cross-token falling walk allowing gaps),
//! builds a per-doc hit dictionary grouped by token position, then filters
//! by position compactness.
//!
//! No DFA. No word_ids. No concat bytes. Just trigrams and positions.

use std::collections::{HashMap, HashSet};

use common::BitSet;

use crate::query::phrase_query::literal_pipeline;
use crate::query::phrase_query::literal_resolve::LiteralMatch;
use crate::query::posting_resolver::PostingResolver;
use crate::suffix_fst::file::SfxFileReader;
use crate::DocId;

// ─────────────────────────────────────────────────────────────────────
// Brique 1 : concat_query — strip separators, lowercase
// ─────────────────────────────────────────────────────────────────────

/// Strip all non-alphanumeric characters and lowercase.
/// "rag3db_value_destroy" → "rag3dbvaluedestroy"
/// "rag3db is cool"       → "rag3dbiscool"
fn concat_query(query: &str) -> String {
    query
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect()
}

/// Count the number of alphanumeric words in the original query.
/// "rag3db_value_destroy" → 3
/// "rag3db is cool"       → 3
/// "rag3weaver"           → 1
fn count_words(query: &str) -> usize {
    query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .count()
}

// ─────────────────────────────────────────────────────────────────────
// Brique 2 : generate_trigrams — sliding window on concatenated query
// ─────────────────────────────────────────────────────────────────────

/// Generate n-grams from the concatenated query.
/// Returns (ngram_strings, query_positions, ngram_size).
fn generate_trigrams(concat: &str, distance: u8) -> (Vec<String>, Vec<usize>, usize) {
    let len = concat.len();
    let n = if len >= 3 * (distance as usize + 1) + 1 { 3 } else { 2 };

    let mut ngrams = Vec::new();
    let mut positions = Vec::new();

    if len < n {
        // Query too short for any n-gram — use the whole thing
        if !concat.is_empty() {
            ngrams.push(concat.to_string());
            positions.push(0);
        }
        return (ngrams, positions, n);
    }

    let bytes = concat.as_bytes();
    for i in 0..=bytes.len() - n {
        if !concat.is_char_boundary(i) || !concat.is_char_boundary(i + n) {
            continue;
        }
        ngrams.push(concat[i..i + n].to_string());
        positions.push(i);
    }

    (ngrams, positions, n)
}

// ─────────────────────────────────────────────────────────────────────
// Brique 3 : TrigramHit — per-doc match with token decomposition
// ─────────────────────────────────────────────────────────────────────

/// A trigram match at a specific token position in a document.
#[derive(Debug, Clone)]
pub struct TrigramHit {
    /// Which trigram of the query (index into the ngrams list).
    pub tri_idx: usize,
    /// Token index in the document (0-based).
    pub position: u32,
    /// Content byte offset where this trigram match starts.
    pub byte_from: u32,
    /// Content byte offset where this trigram match ends.
    pub byte_to: u32,
    /// Suffix index within the parent token.
    pub si: u16,
    /// Decomposition of the trigram across tokens.
    /// Single-token: ["ag3"]
    /// Cross-token:  ["b", "va"] (end of one token + start of next)
    pub token_parts: Vec<String>,
}

/// All trigram hits for one document, grouped by token position.
pub type DocHits = HashMap<u32, Vec<TrigramHit>>;

/// The complete hit dictionary: doc_id → position → hits.
pub type HitsByDoc = HashMap<DocId, DocHits>;

// ─────────────────────────────────────────────────────────────────────
// Brique 4 : build_hits_by_doc — construct the hit dictionary
// ─────────────────────────────────────────────────────────────────────

/// Build the hit dictionary from resolved matches.
///
/// `all_single_matches[i]` = single-token LiteralMatch for trigram i.
/// `all_cross_matches[i]`  = (LiteralMatch, token_parts) for cross-token trigram i.
/// `ngrams[i]`             = the trigram string (for single-token token_parts).
fn build_hits_by_doc(
    ngrams: &[String],
    all_single_matches: &[Vec<LiteralMatch>],
    all_cross_matches: &[Vec<(LiteralMatch, Vec<String>)>],
) -> HitsByDoc {
    let mut hits: HitsByDoc = HashMap::new();

    for (tri_idx, matches) in all_single_matches.iter().enumerate() {
        for m in matches {
            hits.entry(m.doc_id)
                .or_default()
                .entry(m.position)
                .or_default()
                .push(TrigramHit {
                    tri_idx,
                    position: m.position,
                    byte_from: m.byte_from,
                    byte_to: m.byte_to,
                    si: m.si,
                    token_parts: vec![ngrams[tri_idx].clone()],
                });
        }
    }

    for (tri_idx, matches) in all_cross_matches.iter().enumerate() {
        for (m, parts) in matches {
            hits.entry(m.doc_id)
                .or_default()
                .entry(m.position)
                .or_default()
                .push(TrigramHit {
                    tri_idx,
                    position: m.position,
                    byte_from: m.byte_from,
                    byte_to: m.byte_to,
                    si: m.si,
                    token_parts: parts.clone(),
                });
        }
    }

    hits
}

// ─────────────────────────────────────────────────────────────────────
// Brique 5 : find_matches — position-based filtering
// ─────────────────────────────────────────────────────────────────────

/// A validated fuzzy match in a document.
#[derive(Debug, Clone)]
struct FuzzyMatch {
    doc_id: DocId,
    byte_from: u32,
    byte_to: u32,
    /// Ratio of matched trigrams to total trigrams (0.0 - 1.0).
    coverage: f32,
}

/// Find all matches in the hit dictionary by anchoring on positions.
///
/// For each document, for each position P, count how many distinct tri_idx
/// appear in the zone [P, P + max_span]. If >= threshold, emit a match.
fn find_matches(
    hits_by_doc: &HitsByDoc,
    threshold: usize,
    max_span: u32,
    query_positions: &[usize],
    concat_len: usize,
    ngram_size: usize,
    total_ngrams: usize,
) -> Vec<FuzzyMatch> {
    let mut results = Vec::new();

    for (&doc_id, doc_hits) in hits_by_doc {
        // Collect all positions that have hits, sorted
        let mut positions: Vec<u32> = doc_hits.keys().copied().collect();
        positions.sort_unstable();

        for &anchor in &positions {
            // Count distinct tri_idx in [anchor, anchor + max_span]
            let mut seen_tri = HashSet::new();
            let mut min_bf = u32::MAX;
            let mut max_bf: u32 = 0;
            let mut best_first_tri: usize = usize::MAX;
            let mut best_last_tri: usize = 0;

            for &pos in &positions {
                if pos < anchor { continue; }
                if pos > anchor + max_span { break; }

                if let Some(hits) = doc_hits.get(&pos) {
                    for hit in hits {
                        seen_tri.insert(hit.tri_idx);
                        if hit.byte_from < min_bf {
                            min_bf = hit.byte_from;
                        }
                        if hit.byte_from > max_bf {
                            max_bf = hit.byte_from;
                        }
                        if hit.tri_idx < best_first_tri {
                            best_first_tri = hit.tri_idx;
                        }
                        if hit.tri_idx > best_last_tri {
                            best_last_tri = hit.tri_idx;
                        }
                    }
                }
            }

            if seen_tri.len() >= threshold {
                // Compute highlight from the first and last trigram
                let hl_start = min_bf.saturating_sub(query_positions[best_first_tri] as u32);
                let remaining = concat_len.saturating_sub(query_positions[best_last_tri] + ngram_size);
                let hl_end = max_bf + ngram_size as u32 + remaining as u32;

                let coverage = seen_tri.len() as f32 / total_ngrams.max(1) as f32;
                results.push(FuzzyMatch {
                    doc_id,
                    byte_from: hl_start,
                    byte_to: hl_end,
                    coverage,
                });
            }
        }
    }

    results
}

// ─────────────────────────────────────────────────────────────────────
// Brique 6 : resolve_trigram_cross_token — cross-token resolve with token_parts
// ─────────────────────────────────────────────────────────────────────

/// Resolve cross-token chains and build (LiteralMatch, token_parts) pairs.
///
/// For each chain, resolve postings and extract the token decomposition
/// from the chain ordinals via ord_to_term.
fn resolve_cross_with_parts(
    chains: &[literal_pipeline::CrossTokenChain],
    trigram: &str,
    resolver: &dyn PostingResolver,
    ord_to_term: &dyn Fn(u64) -> Option<String>,
    filter_docs: Option<&HashSet<DocId>>,
) -> Vec<(LiteralMatch, Vec<String>)> {
    let literal_len = trigram.len();

    // Resolve chains the normal way
    let matches = literal_pipeline::resolve_chains(chains, literal_len, resolver, filter_docs);

    // For each match, find which chain produced it and extract token_parts
    let mut results = Vec::new();

    for m in matches {
        // Find the chain that produced this match by matching ordinal
        let chain = chains.iter().find(|c| c.ordinals[0] as u32 == m.ordinal);

        let parts = if let Some(chain) = chain {
            // Build token_parts from chain ordinals
            let mut parts = Vec::new();
            let mut remaining = trigram.to_lowercase();

            for (i, &ord) in chain.ordinals.iter().enumerate() {
                if let Some(text) = ord_to_term(ord) {
                    if i == 0 {
                        // First token: trigram starts at SI, consumes prefix_len bytes
                        let consume = chain.prefix_len.min(remaining.len());
                        parts.push(remaining[..consume].to_string());
                        remaining = remaining[consume..].to_string();
                    } else {
                        // Subsequent tokens: consume from start of token
                        let consume = text.len().min(remaining.len());
                        parts.push(remaining[..consume].to_string());
                        remaining = remaining[consume..].to_string();
                    }
                }
            }

            if !remaining.is_empty() {
                // Shouldn't happen but handle gracefully
                if let Some(last) = parts.last_mut() {
                    last.push_str(&remaining);
                }
            }

            parts
        } else {
            // Fallback: single part
            vec![trigram.to_lowercase()]
        };

        results.push((m, parts));
    }

    results
}

// ─────────────────────────────────────────────────────────────────────
// Brique 7 : fuzzy_contains — the full pipeline
// ─────────────────────────────────────────────────────────────────────

/// Fuzzy contains search. Independent pipeline from regex contains.
///
/// 1. Concatenate query (strip separators)
/// 2. Generate trigrams
/// 3. Resolve each trigram (selective by doc, rarest first)
/// 4. Build hit dictionary per doc, grouped by position
/// 5. Filter by position compactness (anchor-based)
/// 6. Compute highlights
///
/// Returns (doc_bitset, highlights).
pub fn fuzzy_contains(
    query_text: &str,
    distance: u8,
    sfx_reader: &SfxFileReader,
    resolver: &dyn PostingResolver,
    ord_to_term: &dyn Fn(u64) -> Option<String>,
    max_doc: DocId,
) -> crate::Result<(BitSet, Vec<(DocId, usize, usize)>, Vec<(DocId, f32)>)> {
    fuzzy_contains_inner(query_text, distance, sfx_reader, resolver, ord_to_term, max_doc, true)
}

fn fuzzy_contains_inner(
    query_text: &str,
    distance: u8,
    sfx_reader: &SfxFileReader,
    resolver: &dyn PostingResolver,
    ord_to_term: &dyn Fn(u64) -> Option<String>,
    max_doc: DocId,
    compute_coverage: bool,
) -> crate::Result<(BitSet, Vec<(DocId, usize, usize)>, Vec<(DocId, f32)>)> {
    let _t_total = std::time::Instant::now();

    // Step 1: Concatenate query
    let concat = concat_query(query_text);
    if concat.is_empty() {
        return Ok((BitSet::with_max_value(max_doc), Vec::new(), Vec::new()));
    }
    let num_words = count_words(query_text);

    // Step 2: Generate trigrams
    let (ngrams, query_positions, n) = generate_trigrams(&concat, distance);
    if ngrams.is_empty() {
        return Ok((BitSet::with_max_value(max_doc), Vec::new(), Vec::new()));
    }

    // Threshold: pigeonhole principle.
    // - n*d trigrams can be broken by d edits
    // - (n-1)*num_boundaries trigrams are broken by word boundaries
    //   (cross-token falling walk with contiguous siblings only won't find them)
    let num_boundaries = num_words.saturating_sub(1);
    let broken_by_boundaries = (n as i32 - 1) * num_boundaries as i32;
    let threshold = (ngrams.len() as i32 - n as i32 * distance as i32 - broken_by_boundaries).max(2) as usize;
    let max_span = num_words as u32 + distance as u32;

    // Step 3: Resolve trigrams — selective by doc, rarest first.
    //
    // Phase A: FST walk + falling walk (no resolve) to estimate selectivity.
    let mut fst_cands_per: Vec<Vec<literal_pipeline::FstCandidate>> = Vec::new();
    let mut ct_chains_per: Vec<Vec<literal_pipeline::CrossTokenChain>> = Vec::new();
    let mut selectivity: Vec<(usize, usize)> = Vec::new();

    for (i, gram) in ngrams.iter().enumerate() {
        let fst_cands = literal_pipeline::fst_candidates(sfx_reader, gram);
        let ct_chains = literal_pipeline::cross_token_falling_walk(sfx_reader, gram, 0, ord_to_term);
        let score = fst_cands.len() + ct_chains.len();
        selectivity.push((i, score));
        fst_cands_per.push(fst_cands);
        ct_chains_per.push(ct_chains);
    }

    selectivity.sort_by_key(|&(_, score)| score);

    // Phase B: Resolve rarest first, build doc filter, resolve rest with filter.
    let mut all_single_matches: Vec<Vec<LiteralMatch>> = vec![Vec::new(); ngrams.len()];
    let mut all_cross_matches: Vec<Vec<(LiteralMatch, Vec<String>)>> = vec![Vec::new(); ngrams.len()];
    let mut doc_filter: Option<HashSet<DocId>> = None;

    // Resolve all trigrams (rarest first without filter, rest with filter)
    let filter_count = selectivity.iter()
        .filter(|&&(idx, _)| !fst_cands_per[idx].is_empty() || !ct_chains_per[idx].is_empty())
        .count();

    let exact_grams: Vec<(usize, usize)> = selectivity.iter()
        .filter(|&&(idx, _)| !fst_cands_per[idx].is_empty() || !ct_chains_per[idx].is_empty())
        .copied()
        .collect();

    // B1: Resolve all exact trigrams without filter (for doc union)
    for &(gram_idx, _) in exact_grams.iter().take(filter_count) {
        let literal_len = ngrams[gram_idx].len();

        let singles = literal_pipeline::resolve_candidates(
            &fst_cands_per[gram_idx], literal_len, resolver, None,
        );
        let crosses = resolve_cross_with_parts(
            &ct_chains_per[gram_idx], &ngrams[gram_idx], resolver, ord_to_term, None,
        );

        let gram_docs: HashSet<DocId> = singles.iter().map(|m| m.doc_id)
            .chain(crosses.iter().map(|(m, _)| m.doc_id))
            .collect();

        doc_filter = Some(match doc_filter {
            None => gram_docs,
            Some(mut prev) => { prev.extend(gram_docs); prev },
        });

        all_single_matches[gram_idx] = singles;
        all_cross_matches[gram_idx] = crosses;
    }

    // B2: Resolve remaining with doc filter
    for &(gram_idx, _) in &selectivity {
        if !all_single_matches[gram_idx].is_empty() || !all_cross_matches[gram_idx].is_empty() {
            continue;
        }

        let literal_len = ngrams[gram_idx].len();
        let filter_ref = doc_filter.as_ref();

        let singles = literal_pipeline::resolve_candidates(
            &fst_cands_per[gram_idx], literal_len, resolver, filter_ref,
        );
        let crosses = resolve_cross_with_parts(
            &ct_chains_per[gram_idx], &ngrams[gram_idx], resolver, ord_to_term, filter_ref,
        );

        all_single_matches[gram_idx] = singles;
        all_cross_matches[gram_idx] = crosses;
    }

    // Step 4: Build hit dictionary
    let hits_by_doc = build_hits_by_doc(&ngrams, &all_single_matches, &all_cross_matches);

    // Step 5: Find matches by position anchoring
    let matches = find_matches(
        &hits_by_doc, threshold, max_span,
        &query_positions, concat.len(), n, ngrams.len(),
    );

    // Step 6: Build result
    let mut doc_bitset = BitSet::with_max_value(max_doc);
    let mut highlights: Vec<(DocId, usize, usize)> = Vec::new();

    // Build per-doc coverage: best (highest) coverage across all matches in the doc.
    let mut best_coverage: HashMap<DocId, f32> = HashMap::new();
    for m in &matches {
        doc_bitset.insert(m.doc_id);
        highlights.push((m.doc_id, m.byte_from as usize, m.byte_to as usize));
        if compute_coverage {
            let entry = best_coverage.entry(m.doc_id).or_insert(0.0);
            if m.coverage > *entry {
                *entry = m.coverage;
            }
        }
    }

    highlights.sort_by_key(|&(doc, bf, bt)| (doc, bf, bt));
    highlights.dedup();

    let doc_coverage: Vec<(DocId, f32)> = best_coverage.into_iter().collect();

    let _total_ms = _t_total.elapsed().as_millis();
    {
        eprintln!("[fuzzy-contains] q='{}' total={}ms ngrams={} threshold={} max_span={} docs={} highlights={}",
            query_text,
            _total_ms, ngrams.len(), threshold, max_span,
            hits_by_doc.len(), highlights.len());
    }

    Ok((doc_bitset, highlights, doc_coverage))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_concat_query() {
        assert_eq!(concat_query("rag3db_value_destroy"), "rag3dbvaluedestroy");
        assert_eq!(concat_query("rag3db is cool"), "rag3dbiscool");
        assert_eq!(concat_query("3db_val"), "3dbval");
        assert_eq!(concat_query("Hello World!"), "helloworld");
        assert_eq!(concat_query("___"), "");
    }

    #[test]
    fn test_count_words() {
        assert_eq!(count_words("rag3db_value_destroy"), 3);
        assert_eq!(count_words("rag3db is cool"), 3);
        assert_eq!(count_words("rag3weaver"), 1);
        assert_eq!(count_words("3db_val"), 2);
    }

    #[test]
    fn test_generate_trigrams_short() {
        let (ngrams, positions, n) = generate_trigrams("3dbval", 1);
        assert_eq!(n, 2); // len=6 < 7, bigrams
        assert_eq!(ngrams, vec!["3d", "db", "bv", "va", "al"]);
        assert_eq!(positions, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn test_generate_trigrams_long() {
        let (ngrams, positions, n) = generate_trigrams("rag3dbvalue", 1);
        assert_eq!(n, 3); // len=11 >= 7, trigrams
        assert_eq!(ngrams.len(), 9);
        assert_eq!(ngrams[0], "rag");
        assert_eq!(ngrams[4], "dbv");
        assert_eq!(ngrams[8], "lue");
    }

    #[test]
    fn test_generate_trigrams_tiny() {
        let (ngrams, positions, n) = generate_trigrams("ab", 1);
        assert_eq!(n, 2);
        assert_eq!(ngrams, vec!["ab"]);
    }
}
