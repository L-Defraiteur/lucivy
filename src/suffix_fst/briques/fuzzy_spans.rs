//! One definition of "a fuzzy occurrence", shared by the engine and its ground
//! truth.
//!
//! The engine's fuzzy highlights used to be the extent of the trigram chain that
//! found the document — 26 to 40 bytes for a 10-byte query — because only the
//! document set was verified against the text, never the span. A ground truth
//! can only check spans against a definition, and if the engine does not use
//! the same one every comparison is noise. So the definition lives here, once.
//!
//! Semi-global edit distance of `needle` against `hay` (free prefix and suffix
//! in `hay`). Every end offset `e` with `D[e] <= d` is part of a *run* of
//! consecutive such offsets; one occurrence per run, ending at the best `e`
//! (smallest distance, then leftmost) and starting where a deterministic
//! traceback from it lands (diagonal first, then deletion, then insertion).
//! Both sides work in the same byte space — lowercase, separators stripped for
//! relaxed mode — and map back to source offsets themselves.

/// Fuzzy occurrences of `needle` in `hay` as `(start, end, distance)` byte
/// ranges of `hay`, one per run of acceptable end offsets.
pub fn fuzzy_spans(needle: &[u8], hay: &[u8], d: usize) -> Vec<(usize, usize, u32)> {
    let m = needle.len();
    let n = hay.len();
    if m == 0 || n == 0 { return Vec::new(); }

    // Full matrix, rows = needle prefix length, columns = hay prefix length.
    // Windows are short and needles are shorter; clarity over a rolling row.
    let w = n + 1;
    let mut dp = vec![0u32; (m + 1) * w];
    for i in 1..=m { dp[i * w] = i as u32; }
    for i in 1..=m {
        let qb = needle[i - 1];
        for j in 1..=n {
            let cost = u32::from(qb != hay[j - 1]);
            let v = (dp[(i - 1) * w + j] + 1)
                .min(dp[i * w + j - 1] + 1)
                .min(dp[(i - 1) * w + j - 1] + cost);
            dp[i * w + j] = v;
        }
    }

    let mut out = Vec::new();
    let mut j = 1;
    while j <= n {
        if dp[m * w + j] as usize > d { j += 1; continue; }
        // A run of acceptable end offsets: pick the best one.
        let mut best_e = j;
        let mut best_d = dp[m * w + j];
        let mut k = j + 1;
        while k <= n && dp[m * w + k] as usize <= d {
            if dp[m * w + k] < best_d { best_d = dp[m * w + k]; best_e = k; }
            k += 1;
        }
        // Traceback from (m, best_e) to row 0. At equal cost: a match first,
        // then a needle byte dropped (the span stays short), then a
        // substitution, then a hay byte skipped. Substituting before dropping
        // would stretch `int64` for `uint64` into `e … int64` over any junk
        // byte that happens to sit before it.
        let (mut i, mut e) = (m, best_e);
        while i > 0 {
            let here = dp[i * w + e];
            if e > 0 && needle[i - 1] == hay[e - 1] && dp[(i - 1) * w + e - 1] == here {
                i -= 1; e -= 1; continue;
            }
            if dp[(i - 1) * w + e] + 1 == here { i -= 1; continue; }
            if e > 0 && dp[(i - 1) * w + e - 1] + 1 == here { i -= 1; e -= 1; continue; }
            e -= 1;
        }
        out.push((e, best_e, best_d));
        j = k;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::fuzzy_spans;

    #[test]
    fn exact_and_near() {
        assert_eq!(fuzzy_spans(b"rag3weaver", b"xx rag3weaver yy", 1), vec![(3, 13, 0)]);
        assert_eq!(fuzzy_spans(b"rag3weaver", b"rak3weaver", 1), vec![(0, 10, 1)]);
        assert_eq!(fuzzy_spans(b"rag3weaver", b"rag3weavr", 1), vec![(0, 9, 1)]);
        assert_eq!(fuzzy_spans(b"rag3weaver", b"rag3weaverr", 1), vec![(0, 10, 0)]);
    }

    #[test]
    fn two_occurrences_back_to_back() {
        let s = fuzzy_spans(b"rag3weaver", b"rag3weaverrag3weaver", 1);
        assert_eq!(s, vec![(0, 10, 0), (10, 20, 0)]);
    }

    #[test]
    fn nothing_far_away() {
        assert!(fuzzy_spans(b"rag3weaver", b"completely different", 1).is_empty());
    }
}
