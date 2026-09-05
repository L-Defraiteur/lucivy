//! Jaro-Winkler as an optional fuzzy metric.
//!
//! The fuzzy pipeline generates candidates by trigram pigeonhole for an edit
//! distance `d`, rebuilds the source text around each candidate chain, then
//! validates the window (`verify_candidates` in `composite.rs`). By default
//! the validation is Levenshtein: "does the window hold a substring within
//! `d` edits of the needle". With `FuzzyMetric::JaroWinkler` it becomes "does
//! the window hold a substring whose Jaro-Winkler similarity to the needle is
//! at least `min_similarity`".
//!
//! Jaro-Winkler compares two whole strings, not a string against a text: the
//! needle is slid over the window in substrings of the needle's length plus
//! or minus `d` characters, and every group of overlapping substrings that
//! pass the threshold **and** sit within `d` edits yields one occurrence
//! (`jaro_spans`). Recall is that of the edit distance `d` — the metric can
//! only tighten the candidate set, never widen it — which keeps its cost
//! bounded and its answer independent of how the index cut its windows.
//!
//! Similarity is on `char`s of the lowercased window and needle, so a
//! multi-byte character counts once.

/// How a fuzzy candidate window is validated.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum FuzzyMetric {
    /// A substring within the query's edit distance (default).
    #[default]
    Levenshtein,
    /// A substring whose Jaro-Winkler similarity to the query is at least
    /// `min_similarity` (0.0..=1.0; 0.9 is the usual threshold).
    JaroWinkler {
        /// Similarity threshold a window must reach to be accepted.
        min_similarity: f32,
    },
}

/// Jaro similarity of two char slices, in `0.0..=1.0`.
pub fn jaro(a: &[char], b: &[char]) -> f32 {
    if a.is_empty() && b.is_empty() { return 1.0; }
    if a.is_empty() || b.is_empty() { return 0.0; }
    if a == b { return 1.0; }
    let window = (a.len().max(b.len()) / 2).saturating_sub(1);
    let mut a_matched = vec![false; a.len()];
    let mut b_matched = vec![false; b.len()];
    let mut matches = 0usize;
    for (i, &ca) in a.iter().enumerate() {
        let lo = i.saturating_sub(window);
        let hi = (i + window + 1).min(b.len());
        for j in lo..hi {
            if !b_matched[j] && b[j] == ca {
                a_matched[i] = true;
                b_matched[j] = true;
                matches += 1;
                break;
            }
        }
    }
    if matches == 0 { return 0.0; }
    // Transpositions: matched chars of `a` against matched chars of `b`, in order.
    let mut transpositions = 0usize;
    let mut j = 0usize;
    for i in 0..a.len() {
        if !a_matched[i] { continue; }
        while !b_matched[j] { j += 1; }
        if a[i] != b[j] { transpositions += 1; }
        j += 1;
    }
    let m = matches as f32;
    (m / a.len() as f32 + m / b.len() as f32 + (m - (transpositions / 2) as f32) / m) / 3.0
}

/// Jaro-Winkler: Jaro boosted by a common prefix of up to four chars
/// (scaling 0.1), the standard parameters.
pub fn jaro_winkler(a: &[char], b: &[char]) -> f32 {
    let j = jaro(a, b);
    let prefix = a.iter().zip(b.iter()).take(4).take_while(|(x, y)| x == y).count();
    j + prefix as f32 * 0.1 * (1.0 - j)
}

/// Char-level Levenshtein distance of `a` and `b` is at most `d`.
fn within_edits(a: &[char], b: &[char], d: usize) -> bool {
    if a.len().abs_diff(b.len()) > d { return false; }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        let mut row_min = i;
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
            row_min = row_min.min(cur[j]);
        }
        if row_min > d { return false; }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()] <= d
}

/// One definition of "a Jaro-Winkler occurrence", shared by the engine and
/// its ground truth (as `fuzzy_spans` is for Levenshtein).
///
/// A candidate is a char-aligned substring of `hay` whose length is within
/// `slack` chars of the needle's, whose Jaro-Winkler similarity to the
/// needle is at least `min_similarity`, **and which is within `slack`
/// edits of the needle** — the recall the trigram pigeonhole guarantees, so
/// that what is reported does not depend on how the index cut its windows.
/// Overlapping candidates form one group, and each group yields one
/// occurrence: the most similar (ties: the shorter, then the leftmost).
/// Before 6 September 2026 the engine kept one occurrence per candidate
/// window, the best (`best_window`): two occurrences in one window lost one,
/// and no ground truth could be written.
///
/// Returns `(byte start, byte end, similarity)` ranges of `hay`, ascending.
pub fn jaro_spans(needle: &[u8], hay: &[u8], slack: usize, min_similarity: f32) -> Vec<(usize, usize, f32)> {
    let (Ok(needle_s), Ok(hay_s)) = (std::str::from_utf8(needle), std::str::from_utf8(hay)) else { return Vec::new() };
    let n: Vec<char> = needle_s.chars().collect();
    if n.is_empty() { return Vec::new(); }
    let mut wc: Vec<(usize, char)> = hay_s.char_indices().collect();
    let total = hay_s.len();
    wc.push((total, '\0'));
    let count = wc.len() - 1;
    if count == 0 { return Vec::new(); }
    let min_len = n.len().saturating_sub(slack).max(1);
    let max_len = (n.len() + slack).min(count);
    let chars: Vec<char> = wc[..count].iter().map(|&(_, c)| c).collect();

    // Candidates, in start order (then length).
    let mut candidates: Vec<(usize, usize, f32)> = Vec::new();
    for start in 0..count {
        for len in min_len..=max_len {
            if start + len > count { break; }
            let sub = &chars[start..start + len];
            let sim = jaro_winkler(&n, sub);
            if sim < min_similarity || !within_edits(&n, sub, slack) { continue; }
            candidates.push((wc[start].0, wc[start + len].0, sim));
        }
    }

    // Groups of overlapping candidates, one occurrence each.
    let mut out: Vec<(usize, usize, f32)> = Vec::new();
    let mut group_end = 0usize;
    let mut best: Option<(usize, usize, f32)> = None;
    for &(s, e, sim) in &candidates {
        if best.is_some() && s >= group_end {
            out.push(best.take().unwrap());
        }
        group_end = group_end.max(e);
        let better = match best {
            None => true,
            Some((bs, be, bsim)) => sim > bsim
                || (sim == bsim && (e - s < be - bs || (e - s == be - bs && s < bs))),
        };
        if better { best = Some((s, e, sim)); }
    }
    if let Some(b) = best { out.push(b); }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(s: &str) -> Vec<char> { s.chars().collect() }

    #[test]
    fn reference_values() {
        // Classic pairs from the literature, to three decimals.
        let close = |a: f32, b: f32| (a - b).abs() < 0.002;
        assert!(close(jaro(&c("martha"), &c("marhta")), 0.944));
        assert!(close(jaro_winkler(&c("martha"), &c("marhta")), 0.961));
        assert!(close(jaro(&c("dixon"), &c("dicksonx")), 0.767));
        assert!(close(jaro_winkler(&c("dixon"), &c("dicksonx")), 0.813));
        assert_eq!(jaro_winkler(&c("kmalloc"), &c("kmalloc")), 1.0);
        assert_eq!(jaro(&c("abc"), &c("xyz")), 0.0);
        assert_eq!(jaro(&c(""), &c("")), 1.0);
        assert_eq!(jaro(&c("a"), &c("")), 0.0);
    }

    #[test]
    fn winkler_prefix_ranks_a_typo_at_the_end_above_one_at_the_start() {
        let end = jaro_winkler(&c("kmalloc"), &c("kmallok"));
        let start = jaro_winkler(&c("kmalloc"), &c("xmalloc"));
        assert!(end > start, "{end} vs {start}");
        assert!(end > 0.9);
    }

    #[test]
    fn jaro_spans_finds_the_typo_inside_a_line() {
        let hay = b"void *buf = kmaloc(sizeof(struct e1000_adapter), GFP_KERNEL);";
        let spans = jaro_spans(b"kmalloc", hay, 2, 0.9);
        assert_eq!(spans.len(), 1, "{spans:?}");
        let (s, e, sim) = spans[0];
        assert_eq!(&hay[s..e], b"kmaloc");
        assert!(sim > 0.9, "{sim}");
        assert!(jaro_spans(b"kmalloc", b"spin_lock_init(&adapter->lock);", 2, 0.9).is_empty());
    }

    #[test]
    fn jaro_spans_reports_every_occurrence_of_a_window() {
        // Two occurrences in one window: both reported, not the best only.
        // The second group holds "kmallocc" and, inside it, the exact
        // "kmalloc": the group's occurrence is the most similar, the exact one.
        let hay = b"kmaloc(a); x = kmallocc(b);";
        let spans = jaro_spans(b"kmalloc", hay, 2, 0.9);
        let texts: Vec<&[u8]> = spans.iter().map(|&(s, e, _)| &hay[s..e]).collect();
        assert_eq!(texts, vec![&b"kmaloc"[..], &b"kmalloc"[..]], "{spans:?}");
        assert_eq!(spans[1].2, 1.0);
        // The exact word: one occurrence, exactly its bytes, similarity 1.
        let spans = jaro_spans(b"kmalloc", b"kmalloc(x)", 2, 0.9);
        assert_eq!(spans, vec![(0, 7, 1.0)]);
    }

    #[test]
    fn jaro_spans_bounds_recall_by_the_edit_distance() {
        // "spnilock" is a transposition of "spinlock": two Levenshtein edits,
        // yet 0.97 similar (every substring of it near the needle's length is
        // above 0.9 too, and none is within one edit). With slack 1 it is not
        // an occurrence; with slack 2 it is, once.
        let near = jaro_winkler(&c("spinlock"), &c("spnilock"));
        assert!(near > 0.9, "{near}");
        assert!(jaro_spans(b"spinlock", b"x spnilock y", 1, 0.9).is_empty());
        let spans = jaro_spans(b"spinlock", b"x spnilock y", 2, 0.9);
        assert_eq!(spans.len(), 1, "{spans:?}");
        assert_eq!(&b"x spnilock y"[spans[0].0..spans[0].1], b"spnilock");
    }

    #[test]
    fn jaro_spans_are_char_aligned() {
        let hay = "été kmaloc é".as_bytes();
        let spans = jaro_spans(b"kmalloc", hay, 2, 0.9);
        assert_eq!(spans.len(), 1);
        let (s, e, _) = spans[0];
        assert!(std::str::from_utf8(&hay[s..e]).is_ok());
        assert_eq!(&hay[s..e], b"kmaloc");
    }
}
