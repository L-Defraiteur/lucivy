//! Substring automaton — matches `.*query.*` for cross-token continuation.
//!
//! States:
//!   0: start (self-loop on any byte, transitions to 1 on first byte of query)
//!   1..N-1: progressing through query bytes
//!   N: accepting (self-loop on any byte)
//!
//! Implements `lucivy_fst::Automaton` so it can be used with `continuation_score`.

use lucivy_fst::Automaton;

/// A simple substring automaton for exact substring matching.
/// Matches any string containing the query as a contiguous substring.
#[derive(Debug, Clone)]
pub struct SubstringAutomaton {
    /// Query bytes (lowercase).
    query: Vec<u8>,
}

impl SubstringAutomaton {
    pub fn new(query: &str) -> Self {
        Self {
            query: query.to_lowercase().into_bytes(),
        }
    }
}

/// State: how many bytes of the query have been matched so far.
/// Uses KMP-style failure function for correct backtracking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SubstringState {
    /// Number of query bytes matched (0..=query.len()).
    matched: usize,
}

impl Automaton for SubstringAutomaton {
    type State = SubstringState;

    fn start(&self) -> SubstringState {
        SubstringState { matched: 0 }
    }

    fn is_match(&self, state: &SubstringState) -> bool {
        state.matched >= self.query.len()
    }

    fn can_match(&self, _state: &SubstringState) -> bool {
        // Can always potentially match (we can stay at state 0 until we find the start)
        true
    }

    fn accept(&self, state: &SubstringState, byte: u8) -> SubstringState {
        if state.matched >= self.query.len() {
            // Already accepted — stay accepting
            return *state;
        }

        let byte_lower = byte.to_ascii_lowercase();

        // Try to extend the current match
        if self.query[state.matched] == byte_lower {
            return SubstringState { matched: state.matched + 1 };
        }

        // Match failed — backtrack using KMP-style logic
        // Try shorter prefixes of the query that are also suffixes of what we've matched
        let mut fallback = state.matched;
        while fallback > 0 {
            fallback = self.failure(fallback);
            if self.query[fallback] == byte_lower {
                return SubstringState { matched: fallback + 1 };
            }
        }

        // No match at all — stay at start
        SubstringState { matched: 0 }
    }
}

impl SubstringAutomaton {
    /// KMP failure function: longest proper prefix of query[..pos] that is also a suffix.
    fn failure(&self, pos: usize) -> usize {
        // Simple implementation — not cached since queries are short
        let pattern = &self.query[..pos];
        for len in (1..pattern.len()).rev() {
            if pattern[..len] == pattern[pattern.len() - len..] {
                return len;
            }
        }
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_automaton(query: &str, text: &str) -> bool {
        let a = SubstringAutomaton::new(query);
        let mut state = a.start();
        for byte in text.bytes() {
            state = a.accept(&state, byte);
            if a.is_match(&state) {
                return true;
            }
        }
        false
    }

    #[test]
    fn test_basic_match() {
        assert!(run_automaton("function", "myfunction"));
        assert!(run_automaton("function", "function"));
        assert!(run_automaton("function", "dysfunctional"));
        assert!(run_automaton("sched", "scheduler"));
    }

    #[test]
    fn test_no_match() {
        assert!(!run_automaton("function", "func"));
        assert!(!run_automaton("sched", "sche"));
        assert!(!run_automaton("xyz", "abc"));
    }

    #[test]
    fn test_case_insensitive() {
        assert!(run_automaton("function", "FUNCTION"));
        assert!(run_automaton("function", "MyFunction"));
    }

    #[test]
    fn test_kmp_backtrack() {
        // "aab" in "aaab" — needs backtracking
        assert!(run_automaton("aab", "aaab"));
        // "abab" in "ababab"
        assert!(run_automaton("abab", "ababab"));
    }

    #[test]
    fn test_cross_token_simulation() {
        // Simulate: token "sche" then token "duler"
        // The automaton processes "sche", reaches state 4 (matched "sche" of "sched")
        // Then processes "duler", first byte 'd' completes the match
        let a = SubstringAutomaton::new("sched");
        let mut state = a.start();
        for byte in b"sche" {
            state = a.accept(&state, *byte);
        }
        assert!(!a.is_match(&state));
        assert_eq!(state.matched, 4); // matched "sche"

        // Continue with next token
        state = a.accept(&state, b'd');
        assert!(a.is_match(&state)); // matched "sched"!
    }
}
