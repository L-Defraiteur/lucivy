//! Benchmark of the search path: `SparseIndex::search` and
//! `mmap_index::search_mmap` with the default options, then the `wand`
//! loop under several window sizes with pruning on and off, on the same
//! corpus and the same queries, every result checked against brute force.
//!
//! Run with:
//!
//! ```text
//! cargo test --release -p sparse-vector --test bench_wand_compare -- --ignored --nocapture
//! ```
//!
//! Corpus: 50 000 records over 2 000 dimensions, 30 non-zeros per record,
//! dimension popularity ~ 1/sqrt(rank) so a handful of posting lists hold
//! most of the records (dim 0 is present in roughly two thirds of them).
//! Queries: 200 queries of 10..40 dimensions drawn from the same popularity
//! law, positive weights, top-10.
//!
//! Historical reference (same corpus, previous implementation, release):
//! RAM 152 us, mmap 153 us median per query.

use std::cell::Cell;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use sparse_vector::index::{SparseIndex, SparseVector};
use sparse_vector::mmap_index::{self, MmapPostingData};
use sparse_vector::wand::{
    search_with, MmapCursor, Posting, PostingCursor, Postings, PostingsBuilder, RecordId,
    Scratch, SearchOptions, TopKSink, Weight,
};

const N_RECORDS: usize = 50_000;
const N_DIMS: usize = 2_000;
const NNZ: usize = 30;
const N_QUERIES: usize = 200;
const TOP_K: usize = 10;

// ---------------------------------------------------------------------------
// Deterministic data
// ---------------------------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Dimension with popularity decaying like 1/sqrt(rank).
    fn zipf_dim(&mut self) -> u32 {
        let u = self.unit();
        (((N_DIMS as f64) * u * u) as usize).min(N_DIMS - 1) as u32
    }
}

struct Record {
    dims: Vec<(u32, f32)>,
}

fn make_corpus(rng: &mut Rng) -> Vec<Record> {
    (0..N_RECORDS)
        .map(|_| {
            let mut dims: Vec<(u32, f32)> = Vec::with_capacity(NNZ);
            while dims.len() < NNZ {
                let d = rng.zipf_dim();
                if dims.iter().any(|&(x, _)| x == d) {
                    continue;
                }
                // BENCH_SKEW=1: most weights near 0.05, a few near 1.0 (SPLADE-like),
                // which gives suffix ceilings something to prune with.
                let u = rng.unit();
                let u = if std::env::var_os("BENCH_SKEW").is_some() { u.powi(6) } else { u };
                let w = 0.05 + 0.95 * u as f32;
                dims.push((d, w));
            }
            dims.sort_by_key(|&(d, _)| d);
            Record { dims }
        })
        .collect()
}

fn make_queries(rng: &mut Rng) -> Vec<Vec<(u32, f32)>> {
    (0..N_QUERIES)
        .map(|_| {
            let nnz = 10 + (rng.next_u64() % 31) as usize;
            let mut q: Vec<(u32, f32)> = Vec::with_capacity(nnz);
            while q.len() < nnz {
                let d = rng.zipf_dim();
                if q.iter().any(|&(x, _)| x == d) {
                    continue;
                }
                q.push((d, 0.1 + 1.9 * rng.unit() as f32));
            }
            q
        })
        .collect()
}

fn to_sparse(q: &[(u32, f32)]) -> SparseVector {
    SparseVector::new(q.iter().map(|p| p.0).collect(), q.iter().map(|p| p.1).collect())
}

// ---------------------------------------------------------------------------
// Ground truth and comparison
// ---------------------------------------------------------------------------

fn brute_force(postings: &[Postings], q: &[(u32, f32)], k: usize) -> Vec<(u64, f32)> {
    let mut scores = vec![0.0f32; N_RECORDS];
    let mut seen = vec![false; N_RECORDS];
    for &(d, qw) in q {
        for p in postings[d as usize].as_slice() {
            scores[p.id as usize] += qw * p.weight;
            seen[p.id as usize] = true;
        }
    }
    let mut hits: Vec<(u64, f32)> = (0..N_RECORDS)
        .filter(|&i| seen[i])
        .map(|i| (i as u64, scores[i]))
        .collect();
    hits.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap().then(a.0.cmp(&b.0)));
    hits.truncate(k);
    hits
}

/// `Ok(true)` when the two lists differ only by which tied ids sit at the
/// k-th score; `Ok(false)` when identical; `Err` on a genuine disagreement.
fn compare(expected: &[(u64, f32)], got: &[(u64, f32)]) -> Result<bool, String> {
    if expected.len() != got.len() {
        return Err(format!("length {} vs {}", expected.len(), got.len()));
    }
    let mut e = expected.to_vec();
    let mut g = got.to_vec();
    let key = |a: &(u64, f32), b: &(u64, f32)| b.1.partial_cmp(&a.1).unwrap().then(a.0.cmp(&b.0));
    e.sort_by(key);
    g.sort_by(key);
    for (i, (x, y)) in e.iter().zip(&g).enumerate() {
        if (x.1 - y.1).abs() > 1e-4 {
            return Err(format!("score at rank {i}: {} vs {}", x.1, y.1));
        }
    }
    if e.iter().zip(&g).all(|(x, y)| x.0 == y.0) {
        return Ok(false);
    }
    let kth = e.last().map(|h| h.1).unwrap_or(0.0);
    for (x, y) in e.iter().zip(&g) {
        if x.0 != y.0 && ((x.1 - kth).abs() > 1e-6 || (y.1 - kth).abs() > 1e-6) {
            return Err(format!("id {} vs {} (scores {} / {}) not a k-th tie", x.0, y.0, x.1, y.1));
        }
    }
    Ok(true)
}

// ---------------------------------------------------------------------------
// Timing helpers
// ---------------------------------------------------------------------------

struct Stats {
    median: f64,
    mean: f64,
    p90: f64,
    min: f64,
}

fn stats(durations: &[Duration]) -> Stats {
    let mut us: Vec<f64> = durations.iter().map(|d| d.as_secs_f64() * 1e6).collect();
    us.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = us.len();
    Stats {
        median: us[n / 2],
        mean: us.iter().sum::<f64>() / n as f64,
        p90: us[(n * 9 / 10).min(n - 1)],
        min: us[0],
    }
}

fn time_all<F>(queries: &[Vec<(u32, f32)>], mut run: F) -> (Vec<Duration>, Vec<Vec<(u64, f32)>>)
where
    F: FnMut(&[(u32, f32)]) -> Vec<(u64, f32)>,
{
    // Warm-up pass, untimed.
    for q in queries {
        std::hint::black_box(run(q));
    }
    let mut durations = Vec::with_capacity(queries.len());
    let mut results = Vec::with_capacity(queries.len());
    for q in queries {
        let t = Instant::now();
        let r = run(q);
        durations.push(t.elapsed());
        results.push(r);
    }
    (durations, results)
}

fn report(label: &str, durations: &[Duration], expected: &[Vec<(u64, f32)>], got: &[Vec<(u64, f32)>]) {
    let s = stats(durations);
    let mut ties = 0usize;
    for (i, (e, g)) in expected.iter().zip(got).enumerate() {
        match compare(e, g) {
            Ok(true) => ties += 1,
            Ok(false) => {}
            Err(msg) => panic!("{label}: query {i} disagrees with ground truth: {msg}"),
        }
    }
    println!(
        "{label:<36} median {:8.1} us  mean {:8.1} us  p90 {:8.1} us  min {:7.1} us  tie-only diffs {ties}",
        s.median, s.mean, s.p90, s.min
    );
}

/// Cursor wrapper counting the postings the search actually consumed.
struct Counting<'a, C> {
    inner: C,
    consumed: &'a Cell<usize>,
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
        let c = self.consumed;
        self.inner.drain_through(hi, |id, w| {
            c.set(c.get() + 1);
            visit(id, w);
        })
    }
}

/// The window / pruning grid exercised on both storages.
fn grid() -> Vec<(&'static str, SearchOptions)> {
    let mut out = Vec::new();
    for &window in &[1024u64, 4096, 16384] {
        for &pruning in &[true, false] {
            let label = match (window, pruning) {
                (1024, true) => "window 1024, pruning",
                (1024, false) => "window 1024, no prune",
                (4096, true) => "window 4096, pruning",
                (4096, false) => "window 4096, no prune",
                (16384, true) => "window 16384, pruning",
                _ => "window 16384, no prune",
            };
            out.push((label, SearchOptions { pruning, window }));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// The benchmark
// ---------------------------------------------------------------------------

#[test]
#[ignore]
fn bench_wand() {
    let mut rng = Rng(0x5EED_1234_ABCD);
    let corpus = make_corpus(&mut rng);
    let queries = make_queries(&mut rng);

    // --- the index, built by in-order inserts ------------------------------
    let t = Instant::now();
    let mut index = SparseIndex::new();
    for (id, r) in corpus.iter().enumerate() {
        index.insert(id as u64, &to_sparse(&r.dims));
    }
    let index_build = t.elapsed();

    // --- postings keyed by token id, through the builder and through upsert -
    let t = Instant::now();
    let mut builders: Vec<PostingsBuilder> = (0..N_DIMS).map(|_| PostingsBuilder::new()).collect();
    for (id, r) in corpus.iter().enumerate() {
        for &(d, w) in &r.dims {
            builders[d as usize].add(id as u64, w);
        }
    }
    let postings: Vec<Postings> = builders.into_iter().map(PostingsBuilder::build).collect();
    let build_builder = t.elapsed();

    let t = Instant::now();
    let mut upserted: Vec<Postings> = (0..N_DIMS).map(|_| Postings::new()).collect();
    for (id, r) in corpus.iter().enumerate() {
        for &(d, w) in &r.dims {
            upserted[d as usize].upsert(id as u64, w);
        }
    }
    let build_upsert = t.elapsed();
    assert_eq!(postings, upserted, "builder and upsert must agree");
    drop(upserted);
    for p in &postings {
        p.check_invariants().unwrap();
    }

    let total_postings: usize = postings.iter().map(Postings::len).sum();
    let mut lens: Vec<usize> = postings.iter().map(Postings::len).collect();
    lens.sort_unstable_by(|a, b| b.cmp(a));
    println!();
    println!(
        "corpus: {N_RECORDS} records, {N_DIMS} dims, {NNZ} nnz/record, {total_postings} postings; \
         longest lists {:?}, median list {}",
        &lens[..5],
        lens[N_DIMS / 2]
    );
    println!(
        "build: SparseIndex::insert {:.0} ms | PostingsBuilder {:.0} ms | Postings::upsert {:.0} ms",
        index_build.as_secs_f64() * 1e3,
        build_builder.as_secs_f64() * 1e3,
        build_upsert.as_secs_f64() * 1e3
    );
    let avg_q_postings: f64 = queries
        .iter()
        .map(|q| q.iter().map(|&(d, _)| postings[d as usize].len()).sum::<usize>() as f64)
        .sum::<f64>()
        / queries.len() as f64;
    println!("queries: {N_QUERIES}, top-{TOP_K}, avg postings touched per query {avg_q_postings:.0}");

    // --- mmap file ------------------------------------------------------------
    let dir = std::env::var("BENCH_SCRATCH")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    let mmap_path = dir.join(format!("bench_wand_compare_{}.mmap", std::process::id()));
    let t = Instant::now();
    mmap_index::write_mmap_file(&mmap_path, index.postings(), index.dim_reverse(), N_RECORDS as u32).unwrap();
    println!("write_mmap_file: {:.0} ms", t.elapsed().as_secs_f64() * 1e3);
    let data = MmapPostingData::open(&mmap_path).unwrap();
    let dim_map: HashMap<u32, usize> = index.dim_map().clone();

    // --- ground truth --------------------------------------------------------
    let expected: Vec<Vec<(u64, f32)>> = queries.iter().map(|q| brute_force(&postings, q, TOP_K)).collect();

    println!();
    // Public entry points, default options.
    let (d, r) = time_all(&queries, |q| index.search(&to_sparse(q), TOP_K));
    report("RAM   SparseIndex::search (default)", &d, &expected, &r);
    let (d, r) = time_all(&queries, |q| {
        mmap_index::search_mmap(&data, &dim_map, &to_sparse(q), TOP_K, &|_| true)
    });
    report("mmap  search_mmap (default)", &d, &expected, &r);

    // The loop itself, RAM, over the grid.
    let mut scratch = Scratch::new();
    let cursor = |d: u32| postings.get(d as usize).filter(|p| !p.is_empty()).map(|p| p.cursor());
    for (label, options) in grid() {
        let (d, r) = time_all(&queries, |q| {
            search_with(q, |_| true, cursor, TopKSink::new(TOP_K), options, &mut scratch)
        });
        report(&format!("RAM   ({label})"), &d, &expected, &r);
    }

    // The loop itself, mmap, over the grid.
    for (label, options) in grid() {
        let (d, r) = time_all(&queries, |q| {
            search_with(
                q,
                |_| true,
                |d| dim_map.get(&d).and_then(|&i| MmapCursor::open(&data, i as u32)),
                TopKSink::new(TOP_K),
                options,
                &mut scratch,
            )
        });
        report(&format!("mmap  ({label})"), &d, &expected, &r);
    }

    // How much does pruning actually skip? (untimed)
    println!();
    for (label, options) in [
        ("window 1024", SearchOptions { pruning: true, window: 1024 }),
        ("window 4096", SearchOptions::default()),
        ("window 16384", SearchOptions { pruning: true, window: 16384 }),
        ("no pruning", SearchOptions::exhaustive()),
    ] {
        let consumed = Cell::new(0usize);
        for q in &queries {
            let _ = search_with(
                q,
                |_| true,
                |d| cursor(d).map(|c| Counting { inner: c, consumed: &consumed }),
                TopKSink::new(TOP_K),
                options,
                &mut scratch,
            );
        }
        println!(
            "postings consumed per query, {label:<13}: {:8.0} ({:5.1}% of {avg_q_postings:.0})",
            consumed.get() as f64 / queries.len() as f64,
            100.0 * consumed.get() as f64 / (queries.len() as f64 * avg_q_postings)
        );
    }

    let _ = std::fs::remove_file(&mmap_path);
}
