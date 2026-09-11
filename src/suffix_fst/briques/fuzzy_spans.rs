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

/// `dp[m][j]` for every `j` of `hay` (`[0] = m`): the best edit distance of
/// `needle` against a substring of `hay` ending at `j` — the last row of the
/// `fuzzy_spans` matrix, whose runs of values `<= d` are its occurrences.
///
/// Bit-parallel for needles of up to 64 bytes (Myers 1999, in Hyyrö's
/// formulation, search mode: the matrix's row 0 is all zeros), about ten
/// word operations per byte of `hay` whatever the needle; column by column
/// otherwise. Memory: the row itself.
pub fn last_row(needle: &[u8], hay: &[u8]) -> Vec<u32> {
    let m = needle.len();
    let n = hay.len();
    let mut last = vec![0u32; n + 1];
    last[0] = m as u32;
    if m == 0 {
        return last;
    }
    if m <= 64 {
        let mut peq = [0u64; 256];
        for (i, &b) in needle.iter().enumerate() {
            peq[b as usize] |= 1u64 << i;
        }
        let top = 1u64 << (m - 1);
        // Vertical deltas of the current column, +1 / -1 (column 0: +1 all).
        let (mut pv, mut mv) = (!0u64, 0u64);
        let mut score = m as u32;
        for (j, &c) in hay.iter().enumerate() {
            let eq = peq[c as usize];
            let xv = eq | mv;
            let xh = ((eq & pv).wrapping_add(pv) ^ pv) | eq;
            let mut ph = mv | !(xh | pv);
            let mut mh = pv & xh;
            if ph & top != 0 {
                score += 1;
            } else if mh & top != 0 {
                score -= 1;
            }
            // Search mode: row 0 costs nothing, no delta shifted in.
            ph <<= 1;
            mh <<= 1;
            pv = mh | !(xv | ph);
            mv = ph & xv;
            last[j + 1] = score;
        }
        return last;
    }
    // `col[i] = dp[i][j]`, one column at a time.
    let mut col: Vec<u32> = (0..=m as u32).collect();
    for j in 1..=n {
        let hb = hay[j - 1];
        let mut diag = 0u32; // dp[0][j - 1]
        for i in 1..=m {
            let left = col[i]; // dp[i][j - 1]
            let cost = u32::from(needle[i - 1] != hb);
            let v = (col[i - 1] + 1).min(left + 1).min(diag + cost);
            diag = left;
            col[i] = v;
        }
        last[j] = col[m];
    }
    last
}

/// Whether `fuzzy_spans(needle, hay, d)` finds anything: some offset `j >= 1`
/// with `dp[m][j] <= d`. The row is not kept and the scan stops at the first
/// offset that proves it — the cheap test a verifier runs before it pays
/// for the spans.
pub fn within_distance(needle: &[u8], hay: &[u8], d: usize) -> bool {
    let m = needle.len();
    if m == 0 || hay.is_empty() {
        return false;
    }
    // `dp[m][j] <= m` for every `j` (drop the whole needle).
    if m <= d {
        return true;
    }
    if m <= 64 {
        let mut peq = [0u64; 256];
        for (i, &b) in needle.iter().enumerate() {
            peq[b as usize] |= 1u64 << i;
        }
        let top = 1u64 << (m - 1);
        let (mut pv, mut mv) = (!0u64, 0u64);
        let mut score = m;
        for &c in hay {
            let eq = peq[c as usize];
            let xv = eq | mv;
            let xh = ((eq & pv).wrapping_add(pv) ^ pv) | eq;
            let mut ph = mv | !(xh | pv);
            let mut mh = pv & xh;
            if ph & top != 0 {
                score += 1;
            } else if mh & top != 0 {
                score -= 1;
                if score <= d {
                    return true;
                }
            }
            ph <<= 1;
            mh <<= 1;
            pv = mh | !(xv | ph);
            mv = ph & xv;
        }
        return false;
    }
    last_row(needle, hay)[1..].iter().any(|&v| v as usize <= d)
}

/// `fuzzy_spans` for a haystack of any length: the same occurrences, in
/// memory proportional to the needle for the scan instead of needle × hay.
/// A stored value can be megabytes long (`briques::stored`, the index
/// without positions, verifies whole values), and a full matrix of `u32`
/// for a 1 MB value and a 10-byte needle is 44 MB — in a 4 GB WebAssembly
/// heap, per query thread.
///
/// Pass 1 computes the last row of the matrix (the best distance of the
/// needle against a substring ending at each offset) one column at a time;
/// its runs of acceptable offsets are `fuzzy_spans`' own. Pass 2 finds each
/// occurrence's start with the same traceback on a local matrix of the
/// columns it can reach. A cell `(i, j)` depends on no byte before
/// `j - 2i` (its best substring is at most `i + dp[i][j] <= 2i` bytes
/// long), and a traceback from `(m, e)` moves left without moving up at
/// most `d` times (each such move pays one edit), so every cell it reads or
/// compares lies at `j - 2i >= e - 2m - d`: from column `e - 2m - d - 1`
/// on, the local matrix holds the exact values.
pub fn fuzzy_spans_long(needle: &[u8], hay: &[u8], d: usize) -> Vec<(usize, usize, u32)> {
    fuzzy_spans_long_above(needle, hay, d, 1 << 22)
}

/// `fuzzy_spans_long`, taking the full-matrix path below `full_cells`
/// cells (the tests force the long path with 0).
fn fuzzy_spans_long_above(needle: &[u8], hay: &[u8], d: usize, full_cells: usize) -> Vec<(usize, usize, u32)> {
    let m = needle.len();
    let n = hay.len();
    if m == 0 || n == 0 { return Vec::new(); }
    if (m + 1).saturating_mul(n + 1) <= full_cells {
        return fuzzy_spans(needle, hay, d);
    }
    // Pass 1: `last[j] = dp[m][j]` (bit-parallel for short needles).
    let last = last_row(needle, hay);

    let mut out = Vec::new();
    let mut local: Vec<u32> = Vec::new();
    let mut j = 1;
    while j <= n {
        if last[j] as usize > d { j += 1; continue; }
        let mut best_e = j;
        let mut best_d = last[j];
        let mut k = j + 1;
        while k <= n && last[k] as usize <= d {
            if last[k] < best_d { best_d = last[k]; best_e = k; }
            k += 1;
        }
        // Pass 2: the local matrix over hay[s0..best_e], then the traceback
        // of `fuzzy_spans`, in local columns.
        let s0 = best_e.saturating_sub(2 * m + d + 1);
        let ln = best_e - s0;
        let w = ln + 1;
        local.clear();
        local.resize((m + 1) * w, 0);
        for i in 1..=m { local[i * w] = i as u32; }
        for i in 1..=m {
            let qb = needle[i - 1];
            for jj in 1..=ln {
                let cost = u32::from(qb != hay[s0 + jj - 1]);
                local[i * w + jj] = (local[(i - 1) * w + jj] + 1)
                    .min(local[i * w + jj - 1] + 1)
                    .min(local[(i - 1) * w + jj - 1] + cost);
            }
        }
        debug_assert_eq!(local[m * w + ln], best_d);
        let (mut i, mut e) = (m, ln);
        while i > 0 {
            let here = local[i * w + e];
            if e > 0 && needle[i - 1] == hay[s0 + e - 1] && local[(i - 1) * w + e - 1] == here {
                i -= 1; e -= 1; continue;
            }
            if local[(i - 1) * w + e] + 1 == here { i -= 1; continue; }
            if e > 0 && local[(i - 1) * w + e - 1] + 1 == here { i -= 1; e -= 1; continue; }
            e -= 1;
        }
        out.push((s0 + e, best_e, best_d));
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
    #[test]
    fn the_long_path_finds_what_the_full_matrix_finds() {
        use super::{fuzzy_spans_long, fuzzy_spans_long_above, last_row, within_distance};
        // A small alphabet makes runs, ties and overlaps frequent.
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = || { seed ^= seed << 13; seed ^= seed >> 7; seed ^= seed << 17; seed };
        for case in 0..3000 {
            let alpha = if case % 3 == 0 { 2 } else { 4 };
            let m = 1 + (next() % 9) as usize;
            let n = (next() % 120) as usize;
            let needle: Vec<u8> = (0..m).map(|_| b'a' + (next() % alpha) as u8).collect();
            let hay: Vec<u8> = (0..n).map(|_| b'a' + (next() % alpha) as u8).collect();
            for d in 0..=3usize.min(m) {
                assert_eq!(
                    fuzzy_spans_long_above(&needle, &hay, d, 0),
                    fuzzy_spans(&needle, &hay, d),
                    "needle {:?} hay {:?} d={d}",
                    String::from_utf8_lossy(&needle), String::from_utf8_lossy(&hay),
                );
            }
        }
        // The bit-parallel row and the column loop agree, on both sides of
        // the 64-byte switch.
        for case in 0..2000 {
            let m = 1 + (next() % 80) as usize;
            let n = (next() % 200) as usize;
            let alpha = 2 + (case % 3) as u64;
            let needle: Vec<u8> = (0..m).map(|_| b'a' + (next() % alpha) as u8).collect();
            let hay: Vec<u8> = (0..n).map(|_| b'a' + (next() % alpha) as u8).collect();
            let full = {
                let w = n + 1;
                let mut dp = vec![0u32; (m + 1) * w];
                for i in 1..=m { dp[i * w] = i as u32; }
                for i in 1..=m {
                    for j in 1..=n {
                        let cost = u32::from(needle[i - 1] != hay[j - 1]);
                        dp[i * w + j] = (dp[(i - 1) * w + j] + 1).min(dp[i * w + j - 1] + 1).min(dp[(i - 1) * w + j - 1] + cost);
                    }
                }
                (0..=n).map(|j| dp[m * w + j]).collect::<Vec<u32>>()
            };
            assert_eq!(last_row(&needle, &hay), full, "m={m} n={n}");
        }
        // The early-exit test agrees with the row, on both sides of 64 bytes
        // and when the needle is no longer than the distance.
        for _ in 0..3000 {
            let m = 1 + (next() % 70) as usize;
            let n = (next() % 150) as usize;
            let needle: Vec<u8> = (0..m).map(|_| b'a' + (next() % 3) as u8).collect();
            let hay: Vec<u8> = (0..n).map(|_| b'a' + (next() % 3) as u8).collect();
            for d in 0..=3usize {
                let row = last_row(&needle, &hay);
                let expect = n > 0 && row[1..].iter().any(|&v| v as usize <= d);
                assert_eq!(within_distance(&needle, &hay, d), expect, "m={m} n={n} d={d}");
                assert_eq!(within_distance(&needle, &hay, d), !fuzzy_spans(&needle, &hay, d).is_empty(), "m={m} n={n} d={d}");
            }
        }
        // And on text, with the default threshold crossed.
        let hay = "schedule sched_clock scheduler schdule shcedule ".repeat(20_000);
        let long = fuzzy_spans_long(b"schdule", hay.as_bytes(), 1);
        assert!(!long.is_empty());
        assert_eq!(long, fuzzy_spans(b"schdule", &hay.as_bytes()[..], 1));
    }

}
