//! Regex gap analyzer: parse a regex pattern into literals + typed gaps.
//!
//! Uses `regex-syntax` to parse the pattern into an AST, then walks it
//! to extract literal segments and classify the gaps between them.
//!
//! Gap types:
//! - AcceptAnything: `.*`, `.+`, `.{n,m}` — order check only
//! - ByteRangeCheck: `[a-z]+`, `\d*`, `\w+` — ByteMap validation O(1)/token
//! - DfaValidation: everything else — full DFA validate_path

use regex_syntax::hir::{Hir, HirKind, Class, ClassBytesRange};

/// A typed gap between two literals in a regex pattern.
#[derive(Debug, Clone, PartialEq)]
pub enum GapKind {
    /// Gap accepts any text (e.g., `.*`, `.+`, `.{n,m}`).
    /// Just verify position ordering — no per-token validation needed.
    AcceptAnything,
    /// Gap is a repeated character class (e.g., `[a-z]+`, `\d*`).
    /// Validate via ByteMap: all bytes of each intermediate token must
    /// fall within the given byte ranges.
    ByteRangeCheck(Vec<(u8, u8)>),
    /// Gap needs full DFA validation (alternations, backreferences, etc.).
    DfaValidation,
}

/// A segment of a parsed regex: either a literal or a gap.
#[derive(Debug, Clone)]
pub enum RegexSegment {
    Literal(String),
    Gap(GapKind),
}

/// Parse a regex pattern into a sequence of literals and typed gaps.
///
/// Returns (literals, gap_kinds) where:
/// - literals[i] is the i-th literal string (lowercased)
/// - gap_kinds[i] is the gap AFTER literals[i] (between literals[i] and literals[i+1])
/// - gap_kinds.len() == literals.len() - 1 for 2+ literals
///
/// Falls back to the simple extractor if regex-syntax parsing fails.
pub fn analyze_regex(pattern: &str) -> (Vec<String>, Vec<GapKind>) {
    let hir = match regex_syntax::parse(pattern) {
        Ok(h) => h,
        Err(_) => return fallback_extract(pattern),
    };

    let segments = walk_hir(&hir);

    // Separate into literals and gaps
    let mut literals: Vec<String> = Vec::new();
    let mut gaps: Vec<GapKind> = Vec::new();
    let mut last_was_literal = false;

    for seg in segments {
        match seg {
            RegexSegment::Literal(s) if !s.is_empty() => {
                if last_was_literal && !literals.is_empty() {
                    // Two consecutive literals with no gap → merge them
                    literals.last_mut().unwrap().push_str(&s);
                } else {
                    literals.push(s);
                    last_was_literal = true;
                }
            }
            RegexSegment::Gap(kind) => {
                if !last_was_literal && !literals.is_empty() {
                    // Two consecutive gaps → merge: if either needs DFA, use DFA
                    if let Some(last_gap) = gaps.last_mut() {
                        *last_gap = merge_gaps(last_gap.clone(), kind);
                    }
                } else if literals.is_empty() {
                    // Gap before first literal — ignore (prefix gap)
                } else {
                    gaps.push(kind);
                    last_was_literal = false;
                }
            }
            _ => {}
        }
    }

    (literals, gaps)
}

/// Walk the HIR tree and produce a flat sequence of segments.
fn walk_hir(hir: &Hir) -> Vec<RegexSegment> {
    match hir.kind() {
        HirKind::Literal(lit) => {
            let s = String::from_utf8_lossy(&lit.0).to_lowercase();
            vec![RegexSegment::Literal(s)]
        }
        HirKind::Concat(subs) => {
            subs.iter().flat_map(walk_hir).collect()
        }
        HirKind::Repetition(rep) => {
            vec![RegexSegment::Gap(classify_repetition(&rep.sub, rep.min, rep.max))]
        }
        HirKind::Class(cls) => {
            // Bare class without repetition (e.g., `[a-z]` = single char)
            // Treat as a gap that needs one char from the class
            if let Some(ranges) = extract_byte_ranges_from_class(cls) {
                vec![RegexSegment::Gap(GapKind::ByteRangeCheck(ranges))]
            } else {
                vec![RegexSegment::Gap(GapKind::DfaValidation)]
            }
        }
        HirKind::Capture(cap) => walk_hir(&cap.sub),
        HirKind::Alternation(_) => {
            vec![RegexSegment::Gap(GapKind::DfaValidation)]
        }
        HirKind::Look(_) => vec![], // anchors — ignore
        HirKind::Empty => vec![],
    }
}

/// Classify a repetition node (*, +, ?, {n,m}).
fn classify_repetition(sub: &Hir, _min: u32, _max: Option<u32>) -> GapKind {
    match sub.kind() {
        // .* or .+ or .{n,m} — accepts any byte
        HirKind::Class(cls) if is_dot_class(cls) => GapKind::AcceptAnything,

        // [a-z]+ or \d* etc — character class repetition
        HirKind::Class(cls) => {
            if let Some(ranges) = extract_byte_ranges_from_class(cls) {
                GapKind::ByteRangeCheck(ranges)
            } else {
                GapKind::DfaValidation
            }
        }

        // Anything else (groups, alternations, nested repetitions)
        _ => GapKind::DfaValidation,
    }
}

/// Check if a character class is essentially `.` (matches any byte).
/// regex-syntax represents `.` as a class with ranges covering all bytes.
fn is_dot_class(cls: &Class) -> bool {
    match cls {
        Class::Bytes(bc) => {
            // `.` typically becomes a single range 0x00..=0xFF or
            // two ranges (0x00..=0x09, 0x0B..=0xFF) depending on flags
            let ranges = bc.ranges();
            if ranges.len() == 1 {
                ranges[0].start() == 0 && ranges[0].end() == 0xFF
            } else {
                // Check if ranges cover at least 250+ bytes (close to full)
                let total: u32 = ranges.iter()
                    .map(|r| (r.end() as u32) - (r.start() as u32) + 1)
                    .sum();
                total >= 250
            }
        }
        Class::Unicode(uc) => {
            // Unicode `.` covers most codepoints
            let ranges = uc.ranges();
            if ranges.len() <= 2 {
                let total: u64 = ranges.iter()
                    .map(|r| (r.end() as u64) - (r.start() as u64) + 1)
                    .sum();
                total >= 0x10000 // covers most of BMP
            } else {
                false
            }
        }
    }
}

/// Extract byte ranges from a character class.
/// Returns None for Unicode classes that don't map cleanly to byte ranges.
fn extract_byte_ranges_from_class(cls: &Class) -> Option<Vec<(u8, u8)>> {
    match cls {
        Class::Bytes(bc) => {
            Some(bc.ranges().iter().map(|r| (r.start(), r.end())).collect())
        }
        Class::Unicode(uc) => {
            // Convert Unicode ranges to byte ranges if all chars are ASCII
            let mut byte_ranges = Vec::new();
            for r in uc.ranges() {
                let start = r.start() as u32;
                let end = r.end() as u32;
                if end > 0x7F {
                    return None; // non-ASCII, can't use bytemap
                }
                byte_ranges.push((start as u8, end as u8));
            }
            Some(byte_ranges)
        }
    }
}

/// Merge two consecutive gaps.
fn merge_gaps(a: GapKind, b: GapKind) -> GapKind {
    match (&a, &b) {
        (GapKind::AcceptAnything, _) | (_, GapKind::AcceptAnything) => GapKind::AcceptAnything,
        (GapKind::DfaValidation, _) | (_, GapKind::DfaValidation) => GapKind::DfaValidation,
        (GapKind::ByteRangeCheck(ra), GapKind::ByteRangeCheck(rb)) => {
            // Union of ranges — conservative
            let mut merged = ra.clone();
            merged.extend(rb);
            GapKind::ByteRangeCheck(merged)
        }
    }
}

/// Fallback: simple extraction when regex-syntax fails.
fn fallback_extract(pattern: &str) -> (Vec<String>, Vec<GapKind>) {
    // Use the existing artisanal parser
    let (lits, gaps) = super::regex_continuation_query::extract_literals_with_gaps(pattern);
    (lits, gaps.into_iter().map(|g| match g {
        super::regex_continuation_query::GapKind::AcceptAnything => GapKind::AcceptAnything,
        super::regex_continuation_query::GapKind::NeedsValidation => GapKind::DfaValidation,
    }).collect())
}

/// Check if ALL bytes of a token (via ByteMap bitmap) fall within the given ranges.
/// Returns true if every set bit in the bitmap corresponds to a byte in one of the ranges.
/// Returns true (conservative) if bytemap doesn't have data for this ordinal.
pub fn token_bytes_in_ranges(
    bytemap: &crate::suffix_fst::bytemap::ByteBitmapReader<'_>,
    ordinal: u32,
    ranges: &[(u8, u8)],
) -> bool {
    let Some(bitmap) = bytemap.bitmap(ordinal) else {
        return true; // no data → conservatively accept
    };

    for chunk_idx in 0..32 {
        let chunk = bitmap[chunk_idx];
        if chunk == 0 { continue; }
        let mut bits = chunk;
        while bits != 0 {
            let bit_pos = bits.trailing_zeros() as u8;
            let byte_val = (chunk_idx as u8) * 8 + bit_pos;
            let in_range = ranges.iter().any(|&(lo, hi)| byte_val >= lo && byte_val <= hi);
            if !in_range { return false; }
            bits &= bits - 1;
        }
    }
    true
}

/// Check if a single byte falls within any of the given ranges.
fn byte_in_ranges(byte: u8, ranges: &[(u8, u8)]) -> bool {
    ranges.iter().any(|&(lo, hi)| byte >= lo && byte <= hi)
}

/// Validate a ByteRangeCheck gap between two positions.
/// Every token between pos_from (exclusive) and pos_to (exclusive) must
/// have all its bytes within the given ranges. Additionally, the separator
/// bytes between tokens (from GapMap) must also be in the ranges — otherwise
/// the regex pattern `[a-z]+` would incorrectly match across newlines,
/// punctuation, etc.
pub fn validate_gap_bytemap(
    posmap: &crate::suffix_fst::posmap::PosMapReader<'_>,
    bytemap: &crate::suffix_fst::bytemap::ByteBitmapReader<'_>,
    gapmap: &crate::suffix_fst::gapmap::GapMapReader<'_>,
    doc_id: crate::DocId,
    pos_from: u32,
    pos_to: u32,
    ranges: &[(u8, u8)],
) -> bool {
    for pos in (pos_from + 1)..pos_to {
        // Check separator bytes between previous token and this one
        let gap = gapmap.read_separator(doc_id, pos - 1, pos);
        if let Some(gap_bytes) = gap {
            if crate::suffix_fst::gapmap::is_value_boundary(gap_bytes) {
                return false; // cross-value boundary
            }
            for &byte in gap_bytes {
                if !byte_in_ranges(byte, ranges) {
                    return false; // separator byte not in allowed ranges
                }
            }
        }

        // Check token bytes via bytemap
        if let Some(ord) = posmap.ordinal_at(doc_id, pos) {
            if !token_bytes_in_ranges(bytemap, ord, ranges) {
                return false;
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_analyze_dot_star() {
        let (lits, gaps) = analyze_regex("rag.*ver");
        assert_eq!(lits, vec!["rag", "ver"]);
        assert_eq!(gaps.len(), 1);
        assert_eq!(gaps[0], GapKind::AcceptAnything);
    }

    #[test]
    fn test_analyze_char_class() {
        let (lits, gaps) = analyze_regex("rag[a-z]+ver");
        assert_eq!(lits, vec!["rag", "ver"]);
        assert_eq!(gaps.len(), 1);
        assert!(matches!(gaps[0], GapKind::ByteRangeCheck(ref r) if r == &[(b'a', b'z')]));
    }

    #[test]
    fn test_analyze_digit_class() {
        let (lits, gaps) = analyze_regex(r"foo\d+bar");
        assert_eq!(lits, vec!["foo", "bar"]);
        assert_eq!(gaps.len(), 1);
        assert!(matches!(gaps[0], GapKind::ByteRangeCheck(ref r) if r.contains(&(b'0', b'9'))));
    }

    #[test]
    fn test_analyze_mixed() {
        let (lits, gaps) = analyze_regex("rag.*mid[a-z]+ver");
        assert_eq!(lits, vec!["rag", "mid", "ver"]);
        assert_eq!(gaps.len(), 2);
        assert_eq!(gaps[0], GapKind::AcceptAnything);
        assert!(matches!(gaps[1], GapKind::ByteRangeCheck(_)));
    }

    #[test]
    fn test_analyze_no_gap() {
        let (lits, gaps) = analyze_regex("ragver");
        assert_eq!(lits, vec!["ragver"]);
        assert_eq!(gaps.len(), 0);
    }

    #[test]
    fn test_analyze_alternation() {
        let (lits, gaps) = analyze_regex("rag(foo|bar)ver");
        assert_eq!(lits.len(), 2);
        assert_eq!(lits[0], "rag");
        assert_eq!(lits[1], "ver");
        assert!(matches!(gaps[0], GapKind::DfaValidation));
    }

    #[test]
    fn test_token_bytes_in_ranges() {
        use crate::suffix_fst::bytemap::{ByteBitmapWriter, ByteBitmapReader};
        let mut writer = ByteBitmapWriter::new();
        writer.ensure_capacity(2);
        writer.record_token(0, b"hello");    // all lowercase
        writer.record_token(1, b"rag3db");   // has digit
        let data = writer.serialize();
        let reader = ByteBitmapReader::open(&data).unwrap();

        // "hello" all in [a-z]
        assert!(token_bytes_in_ranges(&reader, 0, &[(b'a', b'z')]));
        // "rag3db" has '3' which is NOT in [a-z]
        assert!(!token_bytes_in_ranges(&reader, 1, &[(b'a', b'z')]));
        // "rag3db" all in [a-z0-9]
        assert!(token_bytes_in_ranges(&reader, 1, &[(b'a', b'z'), (b'0', b'9')]));
    }
}
