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
//! or minus `d` characters, and the best one wins (`best_window`). Recall
//! stays that of the pigeonhole at distance `d` — the metric can only tighten
//! the candidate set, never widen it — which is what keeps its cost bounded.
//!
//! Similarity is on `char`s of the lowercased window and needle, so a
//! multi-byte character counts once.

/// How a fuzzy candidate window is validated.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FuzzyMetric {
    /// A substring within the query's edit distance (default).
    Levenshtein,
    /// A substring whose Jaro-Winkler similarity to the query is at least
    /// `min_similarity` (0.0..=1.0; 0.9 is the usual threshold).
    JaroWinkler { min_similarity: f32 },
}

impl Default for FuzzyMetric {
    fn default() -> Self { FuzzyMetric::Levenshtein }
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

/// The substring of `window` most similar to `needle`, as
/// `(byte start, byte end, similarity)`, trying every char-aligned start and
/// every length within `needle_chars ± slack`. Both are UTF-8; the returned
/// offsets are byte offsets into `window`, aligned on char boundaries, so
/// they map through the window's back-references like the Levenshtein spans.
pub fn best_window(needle: &[u8], window: &[u8], slack: usize) -> Option<(usize, usize, f32)> {
    let needle_s = std::str::from_utf8(needle).ok()?;
    let window_s = std::str::from_utf8(window).ok()?;
    let n: Vec<char> = needle_s.chars().collect();
    if n.is_empty() { return None; }
    // (byte offset, char) for every char of the window, plus the end sentinel.
    let mut wc: Vec<(usize, char)> = window_s.char_indices().collect();
    let total = window_s.len();
    wc.push((total, '\0'));
    let count = wc.len() - 1;
    if count == 0 { return None; }
    let min_len = n.len().saturating_sub(slack).max(1);
    let max_len = (n.len() + slack).min(count);
    let chars: Vec<char> = wc[..count].iter().map(|&(_, c)| c).collect();
    let mut best: Option<(usize, usize, f32)> = None;
    for start in 0..count {
        for len in min_len..=max_len {
            if start + len > count { break; }
            let sim = jaro_winkler(&n, &chars[start..start + len]);
            if best.map(|(_, _, b)| sim > b).unwrap_or(true) {
                best = Some((wc[start].0, wc[start + len].0, sim));
            }
        }
    }
    best
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
    fn best_window_finds_the_typo_inside_a_line() {
        let window = b"void *buf = kmaloc(sizeof(struct e1000_adapter), GFP_KERNEL);";
        let (s, e, sim) = best_window(b"kmalloc", window, 2).unwrap();
        assert_eq!(&window[s..e], b"kmaloc", "{}", String::from_utf8_lossy(&window[s..e]));
        assert!(sim > 0.95, "{sim}");
        // An unrelated window scores low.
        let (_, _, sim) = best_window(b"kmalloc", b"spin_lock_init(&adapter->lock);", 2).unwrap();
        assert!(sim < 0.8, "{sim}");
    }

    #[test]
    fn best_window_is_char_aligned() {
        let window = "résumé café kmaloc".as_bytes();
        let (s, e, _) = best_window(b"kmalloc", window, 2).unwrap();
        assert!(std::str::from_utf8(&window[s..e]).is_ok());
        assert_eq!(&window[s..e], b"kmaloc");
    }
}
