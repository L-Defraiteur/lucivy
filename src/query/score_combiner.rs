use crate::query::Scorer;
use crate::Score;

/// The `ScoreCombiner` trait defines how to compute
/// an overall score given a list of scores.
pub trait ScoreCombiner: Default + Clone + Send + Copy + 'static {
    /// Aggregates the score combiner with the given scorer.
    ///
    /// The `ScoreCombiner` may decide to call `.scorer.score()`
    /// or not.
    fn update<TScorer: Scorer>(&mut self, scorer: &mut TScorer);

    /// Clears the score combiner state back to its initial state.
    fn clear(&mut self);

    /// Returns the aggregate score.
    fn score(&self) -> Score;
}

/// Just ignores scores. The `DoNothingCombiner` does not
/// even call the scorers `.score()` function.
///
/// It is useful to optimize the case when scoring is disabled.
#[derive(Default, Clone, Copy)] //< these should not be too much work :)
pub struct DoNothingCombiner;

impl ScoreCombiner for DoNothingCombiner {
    fn update<TScorer: Scorer>(&mut self, _scorer: &mut TScorer) {}

    fn clear(&mut self) {}

    #[inline]
    fn score(&self) -> Score {
        1.0
    }
}

/// Sums the score of different scorers.
///
/// The sum is kept in f64 and rounded to a `Score` once: a handful of f32 scores add up exactly in f64, so
/// the result does not depend on the order the scorers come in. That order is not stable — a union takes
/// out a scorer that ran out by `swap_remove` — and an f32 sum in that order gave the same document scores
/// an ulp apart in two indexes of the same documents written in other segments (16 November 2026).
#[derive(Default, Clone, Copy)]
pub struct SumCombiner {
    score: f64,
}

impl ScoreCombiner for SumCombiner {
    fn update<TScorer: Scorer>(&mut self, scorer: &mut TScorer) {
        self.score += scorer.score() as f64;
    }

    fn clear(&mut self) {
        self.score = 0.0;
    }

    #[inline]
    fn score(&self) -> Score {
        self.score as Score
    }
}

/// Take max score of different scorers
/// and optionally sum it with other matches multiplied by `tie_breaker`
///
/// The sum of the other matches is kept in f64, for the reason given at [`SumCombiner`].
#[derive(Default, Clone, Copy)]
pub struct DisjunctionMaxCombiner {
    max: Score,
    sum: f64,
    tie_breaker: Score,
}

impl DisjunctionMaxCombiner {
    /// Creates `DisjunctionMaxCombiner` with tie breaker
    pub fn with_tie_breaker(tie_breaker: Score) -> DisjunctionMaxCombiner {
        DisjunctionMaxCombiner {
            max: 0.0,
            sum: 0.0,
            tie_breaker,
        }
    }
}

impl ScoreCombiner for DisjunctionMaxCombiner {
    fn update<TScorer: Scorer>(&mut self, scorer: &mut TScorer) {
        let score = scorer.score();
        self.max = Score::max(score, self.max);
        self.sum += score as f64;
    }

    fn clear(&mut self) {
        self.max = 0.0;
        self.sum = 0.0;
    }

    #[inline]
    fn score(&self) -> Score {
        self.max + ((self.sum - self.max as f64) as Score) * self.tie_breaker
    }
}

/// **One error budget per document for the words of one query** (lucivy 4.4.2, `contains_split` with a distance).
///
/// A fuzzy `contains` scores `-1000 × errors + BM25` (its verified edits, or its missed trigrams). Summed as a
/// boolean, a word found with a typo cost a tier while a word not found cost nothing: a document that found three
/// words with a typo each ranked below a document that found one word. This combiner reads each clause found, takes
/// back its errors (`ceil(-score / 1000)`) and its BM25, and charges `miss_cost` errors for each clause not found:
/// `-1000 × (errors found + miss_cost × clauses missing) + sum of the BM25`.
#[derive(Clone, Copy)]
pub struct ErrorBudgetCombiner {
    clauses: u32,
    miss_cost: u32,
    matched: u32,
    errors: u32,
    bm25: f64,
}

impl Default for ErrorBudgetCombiner {
    fn default() -> Self {
        ErrorBudgetCombiner::with_budget(0, 1)
    }
}

impl ErrorBudgetCombiner {
    /// `clauses`: the number of words of the query; `miss_cost`: the errors a word not found costs (distance + 1).
    pub fn with_budget(clauses: u32, miss_cost: u32) -> ErrorBudgetCombiner {
        ErrorBudgetCombiner { clauses, miss_cost, matched: 0, errors: 0, bm25: 0.0 }
    }

    /// The errors a fuzzy clause's score carries: 0 for a score of no error, `n` for `-1000 × n + BM25`.
    pub fn errors_of(score: Score) -> u32 {
        if score >= 0.0 { 0 } else { (-score / 1000.0).ceil() as u32 }
    }
}

impl ScoreCombiner for ErrorBudgetCombiner {
    fn update<TScorer: Scorer>(&mut self, scorer: &mut TScorer) {
        let score = scorer.score();
        let errors = Self::errors_of(score);
        self.errors += errors;
        self.bm25 += score as f64 + 1000.0 * errors as f64;
        self.matched += 1;
    }

    fn clear(&mut self) {
        self.matched = 0;
        self.errors = 0;
        self.bm25 = 0.0;
    }

    #[inline]
    fn score(&self) -> Score {
        let missing = self.clauses.saturating_sub(self.matched);
        let errors = self.errors + missing * self.miss_cost;
        (-1000.0 * errors as f64 + self.bm25) as Score
    }
}
