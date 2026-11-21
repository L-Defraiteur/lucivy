use crate::query::{BooleanWeight, EnableScoring, ErrorBudgetCombiner, Occur, Query, Weight};
use crate::Term;

/// **The words of one query, under one error budget per document** (lucivy 4.4.2): a document matches if it finds
/// one word; its score is `ErrorBudgetCombiner`'s, so that finding more words, even with typos, ranks higher, and
/// fewer errors rank higher at an equal number of words. What `contains_split` builds when it has a distance.
#[derive(Debug)]
pub struct ErrorBudgetQuery {
    words: Vec<Box<dyn Query>>,
    miss_cost: u32,
}

impl Clone for ErrorBudgetQuery {
    fn clone(&self) -> Self {
        ErrorBudgetQuery { words: self.words.iter().map(|w| w.box_clone()).collect(), miss_cost: self.miss_cost }
    }
}

impl ErrorBudgetQuery {
    /// `miss_cost`: the errors a word not found costs (the distance + 1).
    pub fn new(words: Vec<Box<dyn Query>>, miss_cost: u32) -> ErrorBudgetQuery {
        ErrorBudgetQuery { words, miss_cost }
    }
}

impl Query for ErrorBudgetQuery {
    fn weight(&self, enable_scoring: EnableScoring<'_>) -> crate::Result<Box<dyn Weight>> {
        let words = self
            .words
            .iter()
            .map(|w| Ok((Occur::Should, w.weight(enable_scoring.clone())?)))
            .collect::<crate::Result<_>>()?;
        let (clauses, miss_cost) = (self.words.len() as u32, self.miss_cost);
        Ok(Box::new(BooleanWeight::new(
            words,
            enable_scoring.is_scoring_enabled(),
            Box::new(move || ErrorBudgetCombiner::with_budget(clauses, miss_cost)),
        )))
    }

    fn query_terms<'a>(&'a self, visitor: &mut dyn FnMut(&'a Term, bool)) {
        for w in &self.words {
            w.query_terms(visitor);
        }
    }
}
