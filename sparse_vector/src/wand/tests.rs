use std::collections::HashMap;

use super::cursor::{PostingCursor, SliceCursor};
use super::frontier::{Frontier, Lane, Skip};
use super::mmap::MmapCursor;
use super::postings::{check_ceilings, Postings, PostingsBuilder};
use super::search::{search, search_with, Scratch, SearchOptions};
use super::sink::{CollectAll, ScoreSink, TopKSink};
use super::{DimId, Posting, RecordId, Weight};

// ---------------------------------------------------------------------------
// Deterministic fixtures
// ---------------------------------------------------------------------------

/// xorshift64* — small, deterministic, dependency-free.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in [0, 1).
    fn unit(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
}

/// A record: id and its sparse vector as `dim -> weight`.
#[derive(Clone, Debug)]
struct Record {
    id: RecordId,
    dims: HashMap<DimId, Weight>,
}

struct Corpus {
    records: Vec<Record>,
    postings: Vec<Postings>,
}

impl Corpus {
    fn cursor(&self, dim: DimId) -> Option<SliceCursor<'_>> {
        self.postings
            .get(dim as usize)
            .filter(|p| !p.is_empty())
            .map(|p| p.cursor())
    }
}

/// `n` records over `dims` dimensions, each record keeping between 10% and
/// 30% of the dimensions; weights in (-1, 1] with roughly a fifth negative.
/// Ids are spread out with irregular gaps so windows are exercised.
fn random_corpus(rng: &mut Rng, n: usize, dims: usize) -> Corpus {
    let mut records = Vec::with_capacity(n);
    let mut next_id: RecordId = 0;
    for _ in 0..n {
        next_id += 1 + rng.below(9);
        if rng.below(20) == 0 {
            next_id += 5_000; // an occasional large gap
        }
        let density = 0.10 + 0.20 * rng.unit();
        let mut record = Record {
            id: next_id,
            dims: HashMap::new(),
        };
        for d in 0..dims as DimId {
            if rng.unit() < density {
                let mut w = 0.05 + rng.unit();
                if rng.below(5) == 0 {
                    w = -w;
                }
                record.dims.insert(d, w);
            }
        }
        records.push(record);
    }
    let postings = build_postings(&records, dims);
    Corpus { records, postings }
}

fn build_postings(records: &[Record], dims: usize) -> Vec<Postings> {
    let mut builders: Vec<PostingsBuilder> = (0..dims).map(|_| PostingsBuilder::new()).collect();
    for r in records {
        for (&d, &w) in &r.dims {
            builders[d as usize].add(r.id, w);
        }
    }
    builders.into_iter().map(PostingsBuilder::build).collect()
}

fn random_query(rng: &mut Rng, dims: usize, negative: bool) -> Vec<(DimId, Weight)> {
    let nnz = 3 + rng.below(10) as usize;
    let mut q = Vec::with_capacity(nnz);
    for _ in 0..nnz {
        let d = rng.below(dims as u64) as DimId;
        if q.iter().any(|&(x, _)| x == d) {
            continue;
        }
        let mut w = 0.1 + 2.0 * rng.unit();
        if negative && rng.below(3) == 0 {
            w = -w;
        }
        q.push((d, w));
    }
    q
}

/// Score exactly as the search does: f32 accumulation in query order.
fn dot(query: &[(DimId, Weight)], record: &Record) -> Option<f32> {
    let mut score = 0.0f32;
    let mut hit = false;
    for &(d, qw) in query {
        if qw == 0.0 {
            continue;
        }
        if let Some(&w) = record.dims.get(&d) {
            score += qw * w;
            hit = true;
        }
    }
    hit.then_some(score)
}

fn brute_force(
    corpus: &Corpus,
    query: &[(DimId, Weight)],
    k: usize,
    filter: impl Fn(RecordId) -> bool,
) -> Vec<(RecordId, f32)> {
    let mut hits: Vec<(RecordId, f32)> = corpus
        .records
        .iter()
        .filter(|r| filter(r.id))
        .filter_map(|r| dot(query, r).map(|s| (r.id, s)))
        .collect();
    hits.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap().then(a.0.cmp(&b.0)));
    hits.truncate(k);
    hits
}

fn assert_same_hits(expected: &[(RecordId, f32)], got: &[(RecordId, f32)], context: &str) {
    assert_eq!(
        expected.len(),
        got.len(),
        "{context}: length differs\nexpected {expected:?}\ngot {got:?}"
    );
    for (i, (e, g)) in expected.iter().zip(got).enumerate() {
        assert_eq!(e.0, g.0, "{context}: id differs at rank {i}\nexpected {expected:?}\ngot {got:?}");
        assert!(
            (e.1 - g.1).abs() <= 1e-5,
            "{context}: score differs at rank {i}: {} vs {}",
            e.1,
            g.1
        );
    }
}

fn run(
    corpus: &Corpus,
    query: &[(DimId, Weight)],
    k: usize,
    options: SearchOptions,
    filter: impl Fn(RecordId) -> bool,
) -> Vec<(RecordId, f32)> {
    let mut scratch = Scratch::new();
    search_with(
        query,
        filter,
        |d| corpus.cursor(d),
        TopKSink::new(k),
        options,
        &mut scratch,
    )
}

// ---------------------------------------------------------------------------
// Ground truth
// ---------------------------------------------------------------------------

#[test]
fn matches_brute_force_with_and_without_pruning() {
    let mut rng = Rng::new(0xC0FFEE);
    let corpus = random_corpus(&mut rng, 200, 50);
    for p in &corpus.postings {
        p.check_invariants().unwrap();
    }

    for round in 0..60 {
        let query = random_query(&mut rng, 50, false);
        for &k in &[1usize, 5, 10, 200] {
            let expected = brute_force(&corpus, &query, k, |_| true);
            let pruned = run(&corpus, &query, k, SearchOptions::default(), |_| true);
            let full = run(&corpus, &query, k, SearchOptions::exhaustive(), |_| true);
            assert_same_hits(&expected, &pruned, &format!("round {round} k {k} pruned"));
            assert_same_hits(&expected, &full, &format!("round {round} k {k} exhaustive"));
        }
    }
}

#[test]
fn matches_brute_force_with_negative_query_weights() {
    let mut rng = Rng::new(0xBAD5EED);
    let corpus = random_corpus(&mut rng, 200, 50);
    for round in 0..40 {
        let query = random_query(&mut rng, 50, true);
        for &k in &[1usize, 5, 10, 200] {
            let expected = brute_force(&corpus, &query, k, |_| true);
            let pruned = run(&corpus, &query, k, SearchOptions::default(), |_| true);
            assert_same_hits(&expected, &pruned, &format!("round {round} k {k} negative"));
        }
    }
}

#[test]
fn window_size_does_not_change_results() {
    let mut rng = Rng::new(42);
    let corpus = random_corpus(&mut rng, 200, 50);
    for round in 0..20 {
        let query = random_query(&mut rng, 50, false);
        let expected = brute_force(&corpus, &query, 10, |_| true);
        for &window in &[1u64, 2, 7, 64, 100_000, u64::MAX] {
            let options = SearchOptions {
                pruning: true,
                window,
            };
            let got = run(&corpus, &query, 10, options, |_| true);
            assert_same_hits(&expected, &got, &format!("round {round} window {window}"));
        }
    }
}

#[test]
fn convenience_search_uses_pruning_and_top_k() {
    let mut rng = Rng::new(7);
    let corpus = random_corpus(&mut rng, 200, 50);
    let query = random_query(&mut rng, 50, false);
    let expected = brute_force(&corpus, &query, 5, |_| true);
    let got = search(&query, 5, |_| true, |d| corpus.cursor(d));
    assert_same_hits(&expected, &got, "search()");
}

#[test]
fn collect_all_sink_returns_every_candidate_sorted() {
    let mut rng = Rng::new(99);
    let corpus = random_corpus(&mut rng, 200, 50);
    let query = random_query(&mut rng, 50, false);
    let expected = brute_force(&corpus, &query, usize::MAX, |_| true);
    let mut scratch = Scratch::new();
    let got = search_with(
        &query,
        |_| true,
        |d| corpus.cursor(d),
        CollectAll::new(),
        SearchOptions::default(),
        &mut scratch,
    );
    assert_same_hits(&expected, &got, "collect all");
}

// ---------------------------------------------------------------------------
// Filter
// ---------------------------------------------------------------------------

#[test]
fn filter_rejecting_even_ids() {
    let mut rng = Rng::new(0xF11);
    let corpus = random_corpus(&mut rng, 200, 50);
    let odd = |id: RecordId| id % 2 == 1;
    for round in 0..30 {
        let query = random_query(&mut rng, 50, false);
        for &k in &[1usize, 5, 10, 200] {
            let expected = brute_force(&corpus, &query, k, odd);
            let got = run(&corpus, &query, k, SearchOptions::default(), odd);
            assert!(got.iter().all(|&(id, _)| id % 2 == 1), "even id leaked");
            assert_same_hits(&expected, &got, &format!("round {round} k {k} odd filter"));
        }
    }
}

#[test]
fn filter_rejecting_everything_yields_nothing() {
    let mut rng = Rng::new(3);
    let corpus = random_corpus(&mut rng, 50, 20);
    let query = random_query(&mut rng, 20, false);
    let got = run(&corpus, &query, 10, SearchOptions::default(), |_| false);
    assert!(got.is_empty());
}

// ---------------------------------------------------------------------------
// Degenerate inputs
// ---------------------------------------------------------------------------

#[test]
fn empty_query_absent_dims_and_empty_postings() {
    let mut rng = Rng::new(11);
    let corpus = random_corpus(&mut rng, 50, 20);

    let empty: Vec<(DimId, Weight)> = Vec::new();
    assert!(search(&empty, 10, |_| true, |d| corpus.cursor(d)).is_empty());

    let absent = vec![(1_000u32, 1.0f32), (2_000, 0.5)];
    assert!(search(&absent, 10, |_| true, |d| corpus.cursor(d)).is_empty());

    let zero_weights = vec![(0u32, 0.0f32), (1, 0.0)];
    assert!(search(&zero_weights, 10, |_| true, |d| corpus.cursor(d)).is_empty());

    let query = random_query(&mut rng, 20, false);
    assert!(search(&query, 0, |_| true, |d| corpus.cursor(d)).is_empty());

    let nothing: Vec<Postings> = vec![Postings::new(), Postings::new()];
    let no_cursor = |d: DimId| nothing.get(d as usize).filter(|p| !p.is_empty()).map(|p| p.cursor());
    assert!(search(&[(0, 1.0), (1, 1.0)], 10, |_| true, no_cursor).is_empty());

    let none = |_: DimId| -> Option<SliceCursor<'static>> { None };
    assert!(search(&query, 10, |_| true, none).is_empty());
}

#[test]
fn single_record_single_dim() {
    let postings = [Postings::from_pairs([(7u64, 0.5f32)])];
    let got = search(&[(0, 2.0)], 10, |_| true, |d| postings.get(d as usize).map(|p| p.cursor()));
    assert_eq!(got, vec![(7, 1.0)]);
}

// ---------------------------------------------------------------------------
// Cursor semantics
// ---------------------------------------------------------------------------

fn sample_postings() -> Postings {
    Postings::from_pairs([(2u64, 0.1f32), (5, 0.9), (9, 0.4), (14, 0.2), (30, 0.7)])
}

#[test]
fn slice_cursor_seek_semantics() {
    let postings = sample_postings();
    let mut c = postings.cursor();
    assert_eq!(c.remaining(), 5);
    assert_eq!(c.last_id(), Some(30));
    assert_eq!(c.peek().map(|p| p.id), Some(2));

    // Present id: lands on it.
    assert_eq!(c.seek(5).map(|p| p.id), Some(5));
    assert_eq!(c.remaining(), 4);
    // Absent id: next larger.
    assert_eq!(c.seek(10).map(|p| p.id), Some(14));
    // Backwards target: no-op, stays on the current element.
    assert_eq!(c.seek(3).map(|p| p.id), Some(14));
    assert_eq!(c.seek(14).map(|p| p.id), Some(14));
    // Advance then seek to the last id.
    c.advance();
    assert_eq!(c.peek().map(|p| p.id), Some(30));
    assert_eq!(c.remaining(), 1);
    // Past the end.
    assert_eq!(c.seek(31), None);
    assert!(c.is_exhausted());
    assert_eq!(c.remaining(), 0);
    assert_eq!(c.upper_bound(), f32::NEG_INFINITY);
    assert_eq!(c.last_id(), Some(30));
    c.advance();
    assert_eq!(c.peek(), None);

    let empty = Postings::new();
    let mut c = empty.cursor();
    assert_eq!(c.peek(), None);
    assert_eq!(c.seek(0), None);
    assert_eq!(c.last_id(), None);
    assert_eq!(c.remaining(), 0);
}

#[test]
fn slice_cursor_upper_bound_tightens_as_it_moves() {
    let postings = sample_postings();
    let mut c = postings.cursor();
    assert_eq!(c.upper_bound(), 0.9);
    c.seek(9);
    assert_eq!(c.upper_bound(), 0.7);
    c.seek(30);
    assert_eq!(c.upper_bound(), 0.7);
    c.advance();
    assert_eq!(c.upper_bound(), f32::NEG_INFINITY);
}

#[test]
fn slice_cursor_drain_through() {
    let postings = sample_postings();
    let mut c = postings.cursor();
    let mut seen = Vec::new();
    c.drain_through(9, |id, w| seen.push((id, w)));
    assert_eq!(seen, vec![(2, 0.1), (5, 0.9), (9, 0.4)]);
    assert_eq!(c.peek().map(|p| p.id), Some(14));
    c.drain_through(13, |id, _| seen.push((id, 0.0)));
    assert_eq!(seen.len(), 3);
    c.drain_through(u64::MAX, |id, _| seen.push((id, 0.0)));
    assert_eq!(seen.len(), 5);
    assert!(c.is_exhausted());
}

// ---------------------------------------------------------------------------
// Ceilings and mutation
// ---------------------------------------------------------------------------

fn assert_ceilings(p: &Postings) {
    p.check_invariants().unwrap();
    // Spell the invariant out independently of the checker.
    let items = p.as_slice();
    for i in 0..items.len() {
        for j in i..items.len() {
            assert!(
                items[i].tail_max >= items[j].weight,
                "tail_max at {i} ({}) below weight at {j} ({})",
                items[i].tail_max,
                items[j].weight
            );
        }
    }
}

#[test]
fn ceilings_hold_after_build() {
    let mut rng = Rng::new(5);
    let corpus = random_corpus(&mut rng, 200, 50);
    for p in &corpus.postings {
        assert_ceilings(p);
    }
    let p = sample_postings();
    let tails: Vec<f32> = p.as_slice().iter().map(|x| x.tail_max).collect();
    assert_eq!(tails, vec![0.9, 0.9, 0.7, 0.7, 0.7]);
}

#[test]
fn builder_sorts_and_keeps_last_duplicate() {
    let mut b = PostingsBuilder::new();
    b.add(9, 1.0).add(3, 2.0).add(9, 5.0).add(1, 0.5);
    let p = b.build();
    let pairs: Vec<(RecordId, f32)> = p.as_slice().iter().map(|x| (x.id, x.weight)).collect();
    assert_eq!(pairs, vec![(1, 0.5), (3, 2.0), (9, 5.0)]);
    assert_ceilings(&p);
}

#[test]
fn upsert_and_delete_round_trips() {
    let mut p = sample_postings();

    // Upsert existing: weight changes, ceilings follow.
    assert_eq!(p.upsert(5, 0.3), Some(0.9));
    assert_eq!(p.get(5), Some(0.3));
    assert_eq!(p.len(), 5);
    let tails: Vec<f32> = p.as_slice().iter().map(|x| x.tail_max).collect();
    assert_eq!(tails, vec![0.7, 0.7, 0.7, 0.7, 0.7]);
    assert_ceilings(&p);

    // Upsert new in the middle raises ceilings before it.
    assert_eq!(p.upsert(10, 2.0), None);
    assert_eq!(p.len(), 6);
    let ids: Vec<RecordId> = p.as_slice().iter().map(|x| x.id).collect();
    assert_eq!(ids, vec![2, 5, 9, 10, 14, 30]);
    let tails: Vec<f32> = p.as_slice().iter().map(|x| x.tail_max).collect();
    assert_eq!(tails, vec![2.0, 2.0, 2.0, 2.0, 0.7, 0.7]);
    assert_ceilings(&p);

    // Upsert at both ends.
    assert_eq!(p.upsert(1, 0.05), None);
    assert_eq!(p.upsert(100, 3.0), None);
    assert_eq!(p.as_slice()[0].tail_max, 3.0);
    assert_ceilings(&p);

    // Delete the maximum: ceilings drop.
    assert_eq!(p.delete(100), Some(3.0));
    assert_eq!(p.as_slice()[0].tail_max, 2.0);
    assert_eq!(p.delete(10), Some(2.0));
    assert_eq!(p.as_slice()[0].tail_max, 0.7);
    assert_ceilings(&p);

    // Delete missing / delete everything.
    assert_eq!(p.delete(10), None);
    for id in [1u64, 2, 5, 9, 14, 30] {
        assert!(p.delete(id).is_some());
        assert_ceilings(&p);
    }
    assert!(p.is_empty());
    assert_eq!(p.delete(2), None);

    // Upsert into an empty list.
    assert_eq!(p.upsert(4, -0.5), None);
    assert_eq!(p.as_slice(), &[Posting::solo(4, -0.5)]);
}

#[test]
fn ceilings_hold_under_random_mutation() {
    let mut rng = Rng::new(0xD1CE);
    let mut p = Postings::new();
    let mut shadow: HashMap<RecordId, f32> = HashMap::new();
    for _ in 0..2_000 {
        let id = rng.below(300);
        if rng.below(3) == 0 {
            assert_eq!(p.delete(id), shadow.remove(&id));
        } else {
            let w = rng.unit() * 2.0 - 0.5;
            assert_eq!(p.upsert(id, w), shadow.insert(id, w));
        }
        assert_ceilings(&p);
        assert_eq!(p.len(), shadow.len());
    }
    // Content matches the shadow map exactly.
    for x in p.as_slice() {
        assert_eq!(shadow.get(&x.id), Some(&x.weight));
    }

    // Bulk edit through items_mut + recompute.
    p.items_mut().iter_mut().for_each(|x| x.weight = -x.weight);
    p.recompute_tail_max();
    assert_ceilings(&p);
}

#[test]
fn in_order_appends_keep_ceilings_and_match_builder() {
    let mut rng = Rng::new(0xA11);
    let pairs: Vec<(RecordId, Weight)> = (0..3_000u64)
        .map(|i| (i * 2, rng.unit() * 2.0 - 0.5))
        .collect();
    let mut appended = Postings::new();
    for &(id, w) in &pairs {
        assert_eq!(appended.upsert(id, w), None);
    }
    assert_ceilings(&appended);
    assert_eq!(appended, Postings::from_sorted_pairs(&pairs));

    // Rising weights: every append raises every ceiling before it.
    let mut rising = Postings::new();
    for i in 0..500u64 {
        rising.upsert(i, i as f32);
    }
    assert_ceilings(&rising);
    assert!(rising.as_slice().iter().all(|p| p.tail_max == 499.0));

    // Re-upserting the same weight is a no-op that reports the old weight.
    let before = rising.clone();
    assert_eq!(rising.upsert(10, 10.0), Some(10.0));
    assert_eq!(rising, before);
}

#[test]
fn duplicate_query_dimensions_are_summed() {
    let mut rng = Rng::new(0xD0B);
    let corpus = random_corpus(&mut rng, 200, 50);
    for round in 0..20 {
        let query = random_query(&mut rng, 50, true);
        // Split every weight in two and add a zero-weight repeat.
        let mut split: Vec<(DimId, Weight)> = Vec::new();
        for &(d, w) in &query {
            split.push((d, w * 0.25));
            split.push((d, 0.0));
            split.push((d, w * 0.75));
        }
        let expected = brute_force(&corpus, &query, 10, |_| true);
        let got = run(&corpus, &split, 10, SearchOptions::default(), |_| true);
        assert_same_hits(&expected, &got, &format!("round {round} split query"));
    }
}

#[test]
fn invariant_checker_catches_violations() {
    let ok = Postings::from_pairs([(1u64, 1.0f32), (2, 2.0)]);
    assert!(check_ceilings(ok.as_slice().iter().copied()).is_ok());

    let bad_ceiling = [Posting { id: 1, weight: 1.0, tail_max: 1.0 }, Posting::solo(2, 2.0)];
    assert!(check_ceilings(bad_ceiling).is_err());

    let bad_order = [Posting::solo(2, 1.0), Posting::solo(1, 1.0)];
    assert!(check_ceilings(bad_order).is_err());
}

// ---------------------------------------------------------------------------
// Frontier
// ---------------------------------------------------------------------------

#[test]
fn frontier_tracks_min_id_and_best_possible() {
    let a = Postings::from_pairs([(1u64, 0.5f32), (4, 1.0), (9, 0.2)]);
    let b = Postings::from_pairs([(3u64, 2.0f32), (4, 0.1)]);
    let c = Postings::new();
    let mut f = Frontier::new(vec![
        Lane::new(1.0, a.cursor()),
        Lane::new(0.5, b.cursor()),
        Lane::new(3.0, c.cursor()),
        Lane::new(0.0, a.cursor()),
    ]);
    assert_eq!(f.len(), 2, "empty and zero-weight lanes are dropped");
    assert_eq!(f.min_id(), Some(1));
    assert_eq!(f.max_last_id(), Some(9));
    assert!((f.best_possible() - (1.0 + 1.0)).abs() < 1e-9);

    // Move lane a past its maximum: the bound tightens.
    f.score_window(1, 4, &mut [0.0; 4], &mut [false; 4]);
    assert_eq!(f.min_id(), Some(9));
    assert!((f.best_possible() - 0.2).abs() < 1e-6);
    f.retire_exhausted();
    assert_eq!(f.len(), 1);
}

#[test]
fn frontier_negative_query_weight_is_unbounded_by_default() {
    let a = Postings::from_pairs([(1u64, 0.5f32), (4, -1.0)]);
    let f = Frontier::new(vec![Lane::new(-1.0, a.cursor())]);
    assert_eq!(f.best_possible(), f64::INFINITY);
}

#[test]
fn frontier_skip_below_moves_to_pivot_and_ends_when_hopeless() {
    // Lane a is weak (headroom 0.3), lane b is strong (headroom 5.0).
    let a = Postings::from_pairs([(1u64, 0.3f32), (2, 0.2), (3, 0.1), (50, 0.05)]);
    let b = Postings::from_pairs([(40u64, 5.0f32), (50, 1.0)]);
    let mut f = Frontier::new(vec![Lane::new(1.0, a.cursor()), Lane::new(1.0, b.cursor())]);

    // Threshold 1.0: ids 1..3 live only in lane a and cannot reach it.
    assert_eq!(f.skip_below(1.0), Skip::Candidates);
    assert_eq!(f.min_id(), Some(40));
    assert_eq!(f.lanes()[0].current_id(), Some(50));
    assert_eq!(f.lanes()[1].current_id(), Some(40));

    // Threshold below every headroom: nothing moves.
    let before: Vec<_> = f.lanes().iter().map(Lane::current_id).collect();
    assert_eq!(f.skip_below(-10.0), Skip::Candidates);
    let after: Vec<_> = f.lanes().iter().map(Lane::current_id).collect();
    assert_eq!(before, after);

    // Threshold above the total headroom (5.05): over.
    assert_eq!(f.skip_below(100.0), Skip::Nothing);
    assert_eq!(f.skip_below(5.06), Skip::Nothing);
    // A threshold within rounding distance of the total is treated as
    // beatable: the guard errs on the side of scoring.
    assert_eq!(f.skip_below(5.05), Skip::Candidates);

    let mut empty: Frontier<SliceCursor<'_>> = Frontier::new(Vec::new());
    assert_eq!(empty.skip_below(0.0), Skip::Nothing);
}

#[test]
fn frontier_score_window_accumulates_in_lane_order() {
    let a = Postings::from_pairs([(10u64, 1.0f32), (12, 2.0)]);
    let b = Postings::from_pairs([(12u64, 3.0f32), (13, 4.0), (20, 1.0)]);
    let mut f = Frontier::new(vec![Lane::new(2.0, a.cursor()), Lane::new(0.5, b.cursor())]);
    let mut scores = [0.0f32; 4];
    let mut seen = [false; 4];
    f.score_window(10, 13, &mut scores, &mut seen);
    assert_eq!(scores, [2.0, 0.0, 4.0 + 1.5, 2.0]);
    assert_eq!(seen, [true, false, true, true]);
    assert_eq!(f.min_id(), Some(20));
}

/// Cursor wrapper counting how many elements the search actually scored.
struct Counting<'a, C> {
    inner: C,
    scored: &'a std::cell::Cell<usize>,
}

impl<C: PostingCursor> PostingCursor for Counting<'_, C> {
    fn peek(&self) -> Option<Posting> {
        self.inner.peek()
    }
    fn advance(&mut self) {
        self.inner.advance()
    }
    fn seek(&mut self, target: RecordId) -> Option<Posting> {
        self.inner.seek(target)
    }
    fn remaining(&self) -> usize {
        self.inner.remaining()
    }
    fn last_id(&self) -> Option<RecordId> {
        self.inner.last_id()
    }
    fn exhaust(&mut self) {
        self.inner.exhaust()
    }
    fn drain_through(&mut self, hi: RecordId, mut visit: impl FnMut(RecordId, Weight)) {
        let scored = self.scored;
        self.inner.drain_through(hi, |id, w| {
            scored.set(scored.get() + 1);
            visit(id, w);
        })
    }
}

#[test]
fn pruning_skips_elements_that_cannot_reach_the_top_k() {
    // 2000 records on one dimension with weights growing with the id
    // (ceiling 1.0), plus a second dimension with weight 10 carried by the
    // first five records and by ten records in the middle. The first
    // window fills the top-5 with scores above 10, so every record living
    // only in the main lane is out of reach: the frontier must seek the
    // main lane straight to the middle boosts (pivot path), then stop once
    // the boost lane is exhausted (nothing-left path).
    let ids: Vec<RecordId> = (0..2000u64).map(|i| i * 3).collect();
    let main = Postings::from_pairs(ids.iter().map(|&id| (id, id as f32 / 6000.0)));
    let boosted = ids[..5].iter().chain(&ids[1000..1010]);
    let boost = Postings::from_pairs(boosted.map(|&id| (id, 10.0f32)));
    let postings = [main, boost];

    let count = |pruning: bool| {
        let scored = std::cell::Cell::new(0usize);
        let options = SearchOptions {
            pruning,
            window: 64,
        };
        let mut scratch = Scratch::new();
        let got = search_with(
            &[(1u32, 1.0f32), (0, 1.0)],
            |_| true,
            |d| {
                postings.get(d as usize).map(|p| Counting {
                    inner: p.cursor(),
                    scored: &scored,
                })
            },
            TopKSink::new(5),
            options,
            &mut scratch,
        );
        (got, scored.get())
    };

    let (pruned, scored_pruned) = count(true);
    let (full, scored_full) = count(false);
    assert_eq!(pruned, full);
    assert_eq!(pruned[0].0, ids[1009]);
    assert_eq!(scored_full, 2000 + 15);
    assert!(
        scored_pruned < 100,
        "pruning scored {scored_pruned} of {scored_full} elements"
    );
}

// ---------------------------------------------------------------------------
// Sinks
// ---------------------------------------------------------------------------

#[test]
fn top_k_sink_orders_and_breaks_ties_on_lower_id() {
    let mut s = TopKSink::new(3);
    assert_eq!(s.threshold(), None);
    s.offer(1, 1.0);
    s.offer(2, 5.0);
    assert_eq!(s.threshold(), None);
    s.offer(3, 3.0);
    assert_eq!(s.threshold(), Some(1.0));
    s.offer(4, 1.0); // tie with the worst, higher id: rejected
    assert_eq!(s.threshold(), Some(1.0));
    s.offer(5, 2.0); // evicts id 1
    assert_eq!(s.threshold(), Some(2.0));
    s.offer(6, 5.0); // tie with the best, retained (evicts 2.0)
    assert_eq!(s.threshold(), Some(3.0));
    assert_eq!(s.into_results(), vec![(2, 5.0), (6, 5.0), (3, 3.0)]);

    // A lower id offered later with a tied score displaces the higher id.
    let mut s = TopKSink::new(1);
    s.offer(9, 1.0);
    s.offer(4, 1.0);
    assert_eq!(s.into_results(), vec![(4, 1.0)]);

    let mut zero = TopKSink::new(0);
    zero.offer(1, 10.0);
    assert_eq!(zero.threshold(), Some(f32::INFINITY));
    assert!(zero.into_results().is_empty());

    let mut all = CollectAll::new();
    all.offer(2, 1.0);
    all.offer(1, 1.0);
    all.offer(3, 0.5);
    assert_eq!(all.threshold(), None);
    assert_eq!(all.into_results(), vec![(1, 1.0), (2, 1.0), (3, 0.5)]);
}

#[test]
fn search_breaks_score_ties_on_lower_id() {
    // Every record scores the same; top-3 must be the three lowest ids.
    let postings = [Postings::from_pairs((0..20u64).map(|i| (i * 3 + 1, 0.5f32)))];
    let got = search(&[(0, 2.0)], 3, |_| true, |d| postings.get(d as usize).map(|p| p.cursor()));
    assert_eq!(got, vec![(1, 1.0), (4, 1.0), (7, 1.0)]);
}

// ---------------------------------------------------------------------------
// Mmap adapter
// ---------------------------------------------------------------------------

struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "sparse-vector-wand-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Write `postings` in the crate's mmap format through its own writer.
fn write_mmap(dir: &TempDir, postings: &[Postings], num_vectors: u32) -> std::path::PathBuf {
    let path = dir.0.join("sparse.mmap");
    // Dimension i is token id i here: what the dense format meant.
    let tokens: Vec<u32> = (0..postings.len() as u32).collect();
    crate::mmap_index::write_mmap_file(&path, postings, &tokens, num_vectors).unwrap();
    path
}

#[test]
fn mmap_adapter_matches_ram_search() {
    let mut rng = Rng::new(0x3A9);
    let corpus = random_corpus(&mut rng, 200, 50);
    let dir = TempDir::new("search");
    let path = write_mmap(&dir, &corpus.postings, corpus.records.len() as u32);
    let data = crate::mmap_index::MmapPostingData::open(&path).unwrap();
    assert_eq!(data.num_dims(), 50);

    // The adapter's ceilings satisfy the invariant, whatever the file's
    // own convention is.
    for dim in 0..50u32 {
        let mut items = Vec::new();
        if let Some(mut c) = MmapCursor::open(&data, dim) {
            while let Some(p) = c.peek() {
                items.push(p);
                c.advance();
            }
        }
        let ram: Vec<(RecordId, f32)> = corpus.postings[dim as usize]
            .as_slice()
            .iter()
            .map(|p| (p.id, p.weight))
            .collect();
        let via_mmap: Vec<(RecordId, f32)> = items.iter().map(|p| (p.id, p.weight)).collect();
        assert_eq!(ram, via_mmap, "dim {dim} content differs");
        check_ceilings(items).unwrap();
    }

    let mut scratch = Scratch::new();
    for round in 0..40 {
        let query = random_query(&mut rng, 50, false);
        for &k in &[1usize, 5, 10, 200] {
            let expected = brute_force(&corpus, &query, k, |_| true);
            let ram = run(&corpus, &query, k, SearchOptions::default(), |_| true);
            let mmap = search_with(
                &query,
                |_| true,
                |d| MmapCursor::open(&data, d),
                TopKSink::new(k),
                SearchOptions::default(),
                &mut scratch,
            );
            assert_same_hits(&expected, &ram, &format!("round {round} k {k} ram"));
            assert_same_hits(&expected, &mmap, &format!("round {round} k {k} mmap"));
        }
    }

    // Filtered, through the convenience entry point.
    let query = random_query(&mut rng, 50, false);
    let odd = |id: RecordId| id % 2 == 1;
    let expected = brute_force(&corpus, &query, 10, odd);
    let got = search(&query, 10, odd, |d| MmapCursor::open(&data, d));
    assert_same_hits(&expected, &got, "mmap odd filter");
}

#[test]
fn mmap_cursor_seek_semantics() {
    let postings = vec![sample_postings(), Postings::new()];
    let dir = TempDir::new("cursor");
    let path = write_mmap(&dir, &postings, 5);
    let data = crate::mmap_index::MmapPostingData::open(&path).unwrap();

    assert!(MmapCursor::open(&data, 1).is_none(), "empty dim has no cursor");
    assert!(MmapCursor::open(&data, 7).is_none(), "unknown dim has no cursor");

    let mut c = MmapCursor::open(&data, 0).unwrap();
    assert_eq!(c.remaining(), 5);
    assert_eq!(c.last_id(), Some(30));
    assert_eq!(c.peek().map(|p| p.id), Some(2));
    assert_eq!(c.upper_bound(), 0.9);
    assert_eq!(c.seek(5).map(|p| p.id), Some(5));
    assert_eq!(c.seek(10).map(|p| p.id), Some(14));
    assert_eq!(c.remaining(), 2);
    assert_eq!(c.seek(3).map(|p| p.id), Some(14), "backwards seek is a no-op");
    assert_eq!(c.upper_bound(), 0.7);
    c.advance();
    assert_eq!(c.peek().map(|p| p.id), Some(30));
    assert_eq!(c.seek(31), None);
    assert!(c.is_exhausted());
    assert_eq!(c.remaining(), 0);
    assert_eq!(c.seek(0), None);

    let mut c = MmapCursor::open(&data, 0).unwrap();
    let mut seen = Vec::new();
    c.drain_through(9, |id, w| seen.push((id, w)));
    assert_eq!(seen, vec![(2, 0.1), (5, 0.9), (9, 0.4)]);
    assert_eq!(c.peek().map(|p| p.id), Some(14));
    c.exhaust();
    assert!(c.is_exhausted());
    assert_eq!(c.remaining(), 0);
}
