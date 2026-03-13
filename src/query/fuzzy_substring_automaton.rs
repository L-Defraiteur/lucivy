use levenshtein_automata::{Distance, DFA, SINK_STATE};
use tantivy_fst::Automaton;

/// Automaton that matches `.*{levenshtein(token, d)}.*` — any FST term
/// containing a substring within Levenshtein distance `d` of the given token.
///
/// Implemented via NFA simulation: at each byte from the FST, we maintain a
/// set of active Levenshtein DFA states, each representing a walk that started
/// at a different byte position. A new walk starts at every byte (the `.*`
/// prefix). Once any walk reaches an accepting state, the automaton is
/// permanently matched (the `.*` suffix).
#[allow(dead_code)]
pub(crate) struct FuzzySubstringAutomaton {
    dfa: DFA,
}

/// State for [`FuzzySubstringAutomaton`].
#[derive(Clone)]
#[allow(dead_code)]
pub struct FuzzySubstringState {
    /// Active Levenshtein DFA states (sorted, deduplicated).
    active: Vec<u32>,
    /// Once true, every extension of the current input also matches.
    matched: bool,
}

#[allow(dead_code)]
impl FuzzySubstringAutomaton {
    /// Build from a pre-built Levenshtein DFA (use the cached
    /// `LevenshteinAutomatonBuilder` to create it).
    pub fn new(dfa: DFA) -> Self {
        FuzzySubstringAutomaton { dfa }
    }
}

impl Automaton for FuzzySubstringAutomaton {
    type State = FuzzySubstringState;

    fn start(&self) -> FuzzySubstringState {
        // Check if the empty string already matches (d >= len(token)).
        let initial = self.dfa.initial_state();
        let matched = matches!(self.dfa.distance(initial), Distance::Exact(_));
        FuzzySubstringState {
            active: Vec::new(),
            matched,
        }
    }

    fn is_match(&self, state: &FuzzySubstringState) -> bool {
        state.matched
    }

    fn can_match(&self, _state: &FuzzySubstringState) -> bool {
        // The `.*` prefix means we can always start a new DFA walk at the
        // next byte, so there is always a chance of matching.
        true
    }

    fn will_always_match(&self, state: &FuzzySubstringState) -> bool {
        // Once matched, the `.*` suffix means every extension also matches.
        state.matched
    }

    fn accept(&self, state: &FuzzySubstringState, byte: u8) -> FuzzySubstringState {
        if state.matched {
            return state.clone();
        }

        let mut new_active = Vec::with_capacity(state.active.len() + 1);

        // Start a new DFA walk from the initial state (= `.*` prefix).
        let initial = self.dfa.initial_state();
        let started = self.dfa.transition(initial, byte);
        if started != SINK_STATE {
            new_active.push(started);
        }

        // Advance all existing active DFA states.
        for &s in &state.active {
            let next = self.dfa.transition(s, byte);
            if next != SINK_STATE {
                new_active.push(next);
            }
        }

        // Sort and deduplicate (multiple walks may converge to the same state).
        new_active.sort_unstable();
        new_active.dedup();

        // Check if any state is accepting.
        let matched = new_active
            .iter()
            .any(|&s| matches!(self.dfa.distance(s), Distance::Exact(_)));

        FuzzySubstringState {
            active: new_active,
            matched,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use levenshtein_automata::LevenshteinAutomatonBuilder;

    fn build(token: &str, distance: u8) -> FuzzySubstringAutomaton {
        let builder = LevenshteinAutomatonBuilder::new(distance, true);
        FuzzySubstringAutomaton::new(builder.build_dfa(token))
    }

    /// Feed a term byte-by-byte and return whether the automaton matches.
    fn matches_term(automaton: &FuzzySubstringAutomaton, term: &str) -> bool {
        let mut state = automaton.start();
        for &b in term.as_bytes() {
            state = automaton.accept(&state, b);
        }
        automaton.is_match(&state)
    }

    #[test]
    fn test_exact_substring() {
        // "program" is an exact substring of "programming"
        let a = build("program", 0);
        assert!(matches_term(&a, "programming"));
        assert!(!matches_term(&a, "hello"));
    }

    #[test]
    fn test_fuzzy_substring() {
        // "progam" d=1: "programming" contains "program" (distance 1 from "progam")
        let a = build("progam", 1);
        assert!(matches_term(&a, "programming"));
    }

    #[test]
    fn test_exact_match() {
        // "hello" matches itself (trivial substring)
        let a = build("hello", 0);
        assert!(matches_term(&a, "hello"));
    }

    #[test]
    fn test_fuzzy_whole_word() {
        // "helo" d=1: "hello" is at distance 1
        let a = build("helo", 1);
        assert!(matches_term(&a, "hello"));
    }

    #[test]
    fn test_no_match() {
        let a = build("xyz", 1);
        assert!(!matches_term(&a, "hello"));
        assert!(!matches_term(&a, "world"));
    }

    #[test]
    fn test_prefix_substring() {
        // "col" is a prefix of "collections"
        let a = build("col", 0);
        assert!(matches_term(&a, "collections"));
    }

    #[test]
    fn test_suffix_substring() {
        // "tions" is a suffix of "collections"
        let a = build("tions", 0);
        assert!(matches_term(&a, "collections"));
    }

    #[test]
    fn test_fuzzy_prefix() {
        // "cll" d=1: "col" (prefix of "collections") is at distance 1
        let a = build("cll", 1);
        assert!(matches_term(&a, "collections"));
    }

    #[test]
    fn test_d0_rejects_fuzzy() {
        // d=0: only exact substrings, no fuzzy
        let a = build("progam", 0);
        assert!(!matches_term(&a, "programming"));
    }

    #[test]
    fn test_will_always_match_once_matched() {
        let a = build("bc", 0);
        let mut state = a.start();
        assert!(!a.will_always_match(&state));

        // Feed "abc" — "bc" is a substring
        for &b in b"abc" {
            state = a.accept(&state, b);
        }
        assert!(a.is_match(&state));
        assert!(a.will_always_match(&state));

        // Further bytes don't change the match status
        state = a.accept(&state, b'd');
        assert!(a.will_always_match(&state));
    }

    #[test]
    fn test_single_char_d1() {
        // "a" d=1: matches empty (distance 1), so everything matches
        let a = build("a", 1);
        assert!(matches_term(&a, "x"));
        assert!(matches_term(&a, "hello"));
    }

    #[test]
    fn test_can_match_always_true() {
        let a = build("test", 1);
        let state = a.start();
        assert!(a.can_match(&state));
    }
}
