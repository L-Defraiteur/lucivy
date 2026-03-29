//! ByteMap-based DFA pre-filter.
//!
//! Before feeding a token's bytes to a DFA (Levenshtein or regex),
//! check if ANY byte in the token can advance the DFA from the current state.
//! If not, the entire token can be skipped — the DFA would die on every byte.
//!
//! Uses the ByteBitmap (256 bits per token ordinal) for O(popcount) checks
//! instead of O(token_len) DFA transitions.

use crate::suffix_fst::bytemap::ByteBitmapReader;
use lucivy_fst::Automaton;

/// Check if a token's bytes can advance the DFA from the given state.
///
/// Returns true if at least one byte in the token's bitmap produces a
/// state where `can_match()` is true. Returns true (conservative) if
/// the bytemap doesn't have data for this ordinal.
///
/// Complexity: O(popcount of bitmap) — typically 5-30 iterations instead
/// of feeding all token bytes to the DFA.
pub fn can_token_advance_dfa<A: Automaton>(
    automaton: &A,
    state: &A::State,
    bytemap: &ByteBitmapReader<'_>,
    ordinal: u32,
) -> bool {
    let Some(bitmap) = bytemap.bitmap(ordinal) else {
        return true; // no bitmap → conservatively assume compatible
    };

    // Iterate only over bytes that are present in the token (via bitmap bits)
    for chunk_idx in 0..32 {
        let chunk = bitmap[chunk_idx];
        if chunk == 0 { continue; }
        // For each set bit in this chunk
        let mut bits = chunk;
        while bits != 0 {
            let bit_pos = bits.trailing_zeros() as u8;
            let byte_val = (chunk_idx as u8) * 8 + bit_pos;
            let next_state = automaton.accept(state, byte_val);
            if automaton.can_match(&next_state) {
                return true;
            }
            bits &= bits - 1; // clear lowest set bit
        }
    }
    false
}

/// Feed a token's bytes to the DFA, but bail early if the bytemap says
/// no byte can advance. Returns the final state after feeding, or None
/// if the DFA died (can_match = false) during the feed.
///
/// If `bytemap` is None, feeds all bytes without pre-filtering.
pub fn feed_token_with_filter<A: Automaton>(
    automaton: &A,
    state: &A::State,
    text: &str,
    bytemap: Option<(&ByteBitmapReader<'_>, u32)>, // (reader, ordinal)
) -> Option<A::State>
where
    A::State: Clone,
{
    // Pre-filter: check if any byte can advance
    if let Some((bm, ord)) = bytemap {
        if !can_token_advance_dfa(automaton, state, bm, ord) {
            return None; // DFA would die on every byte of this token
        }
    }

    // Feed bytes
    let mut s = state.clone();
    for &byte in text.as_bytes() {
        s = automaton.accept(&s, byte);
        if !automaton.can_match(&s) {
            return None;
        }
    }
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suffix_fst::bytemap::ByteBitmapWriter;

    // Simple automaton that only accepts lowercase ASCII
    struct LowerOnly;
    impl Automaton for LowerOnly {
        type State = bool; // true = alive
        fn start(&self) -> bool { true }
        fn is_match(&self, state: &bool) -> bool { *state }
        fn can_match(&self, state: &bool) -> bool { *state }
        fn accept(&self, state: &bool, byte: u8) -> bool {
            *state && byte >= b'a' && byte <= b'z'
        }
    }

    fn make_bytemap(tokens: &[&[u8]]) -> Vec<u8> {
        let mut writer = ByteBitmapWriter::new();
        writer.ensure_capacity(tokens.len() as u32);
        for (ord, text) in tokens.iter().enumerate() {
            writer.record_token(ord as u32, text);
        }
        writer.serialize()
    }

    #[test]
    fn test_compatible_token() {
        let data = make_bytemap(&[b"hello", b"rag3db"]);
        let reader = ByteBitmapReader::open(&data).unwrap();
        let automaton = LowerOnly;
        let state = automaton.start();

        // "hello" = all lowercase → compatible
        assert!(can_token_advance_dfa(&automaton, &state, &reader, 0));
    }

    #[test]
    fn test_incompatible_token() {
        let data = make_bytemap(&[b"12345"]);
        let reader = ByteBitmapReader::open(&data).unwrap();
        let automaton = LowerOnly;
        let state = automaton.start();

        // "12345" = all digits → no byte can advance LowerOnly
        assert!(!can_token_advance_dfa(&automaton, &state, &reader, 0));
    }

    #[test]
    fn test_mixed_token() {
        let data = make_bytemap(&[b"rag3db"]);
        let reader = ByteBitmapReader::open(&data).unwrap();
        let automaton = LowerOnly;
        let state = automaton.start();

        // "rag3db" has lowercase letters → at least one byte can advance
        assert!(can_token_advance_dfa(&automaton, &state, &reader, 0));
    }

    #[test]
    fn test_feed_with_filter_passes() {
        let data = make_bytemap(&[b"hello"]);
        let reader = ByteBitmapReader::open(&data).unwrap();
        let automaton = LowerOnly;
        let state = automaton.start();

        let result = feed_token_with_filter(&automaton, &state, "hello", Some((&reader, 0)));
        assert!(result.is_some());
    }

    #[test]
    fn test_feed_with_filter_skips() {
        let data = make_bytemap(&[b"12345"]);
        let reader = ByteBitmapReader::open(&data).unwrap();
        let automaton = LowerOnly;
        let state = automaton.start();

        let result = feed_token_with_filter(&automaton, &state, "12345", Some((&reader, 0)));
        assert!(result.is_none());
    }

    #[test]
    fn test_feed_without_filter() {
        let automaton = LowerOnly;
        let state = automaton.start();

        let result = feed_token_with_filter::<LowerOnly>(&automaton, &state, "hello", None);
        assert!(result.is_some());

        let result = feed_token_with_filter::<LowerOnly>(&automaton, &state, "12345", None);
        assert!(result.is_none());
    }
}
