//! What a filtered sparse search costs, against searching everything.
//!
//! Asked by the rag3weaver session (doc 08, §2.3 and §2.4): a work domain is
//! a subset — sometimes 1 % of a large index, sometimes 90 % — and they need
//! to know from which side the filter stops paying, and whether a very large
//! allowed set costs something on its own.
//!
//! Two implementations sit behind one heuristic
//! (`index::run_search_allowed`): a **seek** does a binary search per lane
//! for each allowed id, a **window** walks every posting of every lane and
//! tests membership. The first wins when the set is small, the second when
//! it is large; `ids.len() * lanes.len() * 8 < total_postings` picks.
//!
//! Run: `cargo test --release -p sparse-vector --test bench_filter_selectivity -- --ignored --nocapture`

mod corpus_vectors;

use sparse_vector::handle::SparseHandle;

#[test]
#[ignore]
fn filter_cost_by_selectivity() {
    let want: usize = std::env::var("BENCH_DOCS").ok().and_then(|v| v.parse().ok()).unwrap_or(40_000);
    let corpus = corpus_vectors::from_dump(want)
        .unwrap_or_else(|| corpus_vectors::build(want, 200));
    eprintln!("  corpus: {} (x{} replicas)", corpus_vectors::describe(&corpus),
        corpus_vectors::replicas(&corpus));

    let dir = std::env::temp_dir().join("lucivy_sparse_filter_bench");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let h = SparseHandle::create(dir.to_str().unwrap()).unwrap();
    for (id, v) in &corpus.docs { h.insert(*id, v).unwrap(); }
    h.commit_inner().unwrap();
    h.compact().unwrap();   // one segment: the filter is what is measured

    let total = corpus.docs.len();
    let time = |f: &dyn Fn()| {
        for _ in 0..2 { f(); }
        let mut runs: Vec<f64> = (0..5).map(|_| {
            let t = std::time::Instant::now();
            f();
            t.elapsed().as_secs_f64() * 1e3 / corpus.queries.len() as f64
        }).collect();
        runs.sort_by(|a, b| a.partial_cmp(b).unwrap());
        runs[2]
    };

    let unfiltered = time(&|| { for q in &corpus.queries { h.search(q, 10); } });
    eprintln!("  no filter                       {unfiltered:>7.3} ms a query");

    for percent in [0.1f64, 1.0, 5.0, 10.0, 25.0, 50.0, 90.0, 100.0] {
        let keep = ((total as f64) * percent / 100.0).round().max(1.0) as usize;
        let step = (total / keep).max(1);
        let allowed: Vec<u64> = corpus.docs.iter().map(|(id, _)| *id).step_by(step).take(keep).collect();
        // What the heuristic will choose, from the same arithmetic it uses.
        let lanes = corpus.queries.iter().map(|q| q.indices.len()).sum::<usize>() / corpus.queries.len();
        let seek_work = allowed.len() * lanes * 8;
        let filtered = time(&|| { for q in &corpus.queries { h.search_filtered(q, 10, &allowed); } });
        eprintln!("  {percent:>5.1} % allowed ({:>6} ids)   {filtered:>7.3} ms a query · ×{:.2} of unfiltered · seek_work {seek_work}",
            allowed.len(), filtered / unfiltered.max(1e-9));
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// §2.4: does handing over a very large set cost something by itself?
/// The same search, with an allowed set that contains every id of the index
/// and then some — ids that do not exist at all.
#[test]
#[ignore]
fn cost_of_a_very_large_allowed_set() {
    let want: usize = std::env::var("BENCH_DOCS").ok().and_then(|v| v.parse().ok()).unwrap_or(40_000);
    let corpus = corpus_vectors::from_dump(want)
        .unwrap_or_else(|| corpus_vectors::build(want, 200));
    let dir = std::env::temp_dir().join("lucivy_sparse_bigset_bench");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let h = SparseHandle::create(dir.to_str().unwrap()).unwrap();
    for (id, v) in &corpus.docs { h.insert(*id, v).unwrap(); }
    h.commit_inner().unwrap();
    h.compact().unwrap();

    let all: Vec<u64> = corpus.docs.iter().map(|(id, _)| *id).collect();
    let time = |allowed: &[u64]| {
        for _ in 0..2 { for q in &corpus.queries { h.search_filtered(q, 10, allowed); } }
        let mut runs: Vec<f64> = (0..5).map(|_| {
            let t = std::time::Instant::now();
            for q in &corpus.queries { h.search_filtered(q, 10, allowed); }
            t.elapsed().as_secs_f64() * 1e3 / corpus.queries.len() as f64
        }).collect();
        runs.sort_by(|a, b| a.partial_cmp(b).unwrap());
        runs[2]
    };
    for extra in [0usize, 100_000, 500_000] {
        let mut allowed = all.clone();
        allowed.extend((0..extra as u64).map(|i| 50_000_000 + i));   // ids that exist nowhere
        eprintln!("  {:>7} ids ({:>7} of them unknown)  {:>7.3} ms a query",
            allowed.len(), extra, time(&allowed));
    }
    let _ = std::fs::remove_dir_all(&dir);
}
