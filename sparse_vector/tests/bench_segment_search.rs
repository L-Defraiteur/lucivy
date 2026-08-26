//! What a search pays per segment, and what a merge gives back.
//!
//! The answer, on vectors drawn from real text: **nothing measurable**.
//! 0.05 ms on one segment, 0.05 ms on a hundred, reproducible run to run.
//! Splitting the index splits the long posting lists with it, and WAND
//! prunes inside each piece just as well; the per-segment overhead is a
//! binary search per query dimension.
//!
//! It is worth saying what this bench used to claim: with vectors spread
//! uniformly and every weight at 1.0, it read ×5.3 on twenty segments. That
//! number measured the corpus, not the index — flat weights leave WAND
//! nothing to prune with, so every segment restarts a full walk. It is the
//! reason `corpus_vectors` exists.
//!
//! What does grow with the segment count is the **write** path
//! (`update_cost_per_segment` below): an insert or a delete asks every
//! segment whether it holds the id.
//!
//! The vectors come from real text (`corpus_vectors`), not from a uniform
//! spread: WAND's pruning lives on the imbalance between dimensions, and a
//! flat corpus measures something else entirely.
//!
//! Run: `cargo test --release -p sparse-vector --test bench_segment_search -- --ignored --nocapture`

mod corpus_vectors;

use sparse_vector::handle::SparseHandle;

#[test]
#[ignore]
fn search_cost_per_segment() {
    let dir = std::env::temp_dir().join("lucivy_sparse_segment_search");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let h = SparseHandle::create(dir.to_str().unwrap()).unwrap();

    // Real text, so the posting lists and the weights are as unbalanced as
    // a model's — which is the only condition WAND can be measured under.
    let corpus = corpus_vectors::build(40_000, 200);
    eprintln!("  corpus: {}", corpus_vectors::describe(&corpus));
    let total = corpus.docs.len() as u64;
    let queries = corpus.queries.clone();
    let time = |h: &SparseHandle| {
        // warm, then the median of 5 passes over the query set
        for q in &queries { h.search(q, 20); }
        let mut runs: Vec<f64> = (0..5).map(|_| {
            let t = std::time::Instant::now();
            for q in &queries { h.search(q, 20); }
            t.elapsed().as_secs_f64() * 1e3 / queries.len() as f64
        }).collect();
        runs.sort_by(|a, b| a.partial_cmp(b).unwrap());
        runs[2]
    };

    for segments in [1u64, 5, 20, 50, 100] {
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // The policy is what is being measured here, so it must not fire.
        std::env::set_var("LUCIVY_SPARSE_MAX_SEGMENTS", "0");
        let h = SparseHandle::create(dir.to_str().unwrap()).unwrap();
        let per = total / segments;
        for s in 0..segments {
            for i in 0..per {
                let (id, v) = &corpus.docs[((s * per + i) % total) as usize];
                h.insert(*id, v).unwrap();
            }
            h.commit_inner().unwrap();
        }
        let many = time(&h);
        h.compact().unwrap();
        let one = time(&h);
        eprintln!("{segments:>3} segments of {per:>6} — search {many:>7.2} ms · after a merge {one:>7.2} ms · ×{:.1}",
            many / one.max(1e-6));
    }
    drop(h);
    let _ = std::fs::remove_dir_all(&dir);
}

/// What the write path pays per segment. An insert or a delete has to know
/// which segments hold the id, and asks each of them
/// (`Segment::holds` → a binary search in its `.ids`) — that walk is linear
/// in the number of segments, where a search is not.
#[test]
#[ignore]
fn update_cost_per_segment() {
    let corpus = corpus_vectors::build(40_000, 4);
    let dir = std::env::temp_dir().join("lucivy_sparse_update_cost");
    for segments in [1usize, 5, 20, 50, 100] {
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("LUCIVY_SPARSE_MAX_SEGMENTS", "0");
        let h = SparseHandle::create(dir.to_str().unwrap()).unwrap();
        let per = corpus.docs.len() / segments;
        for s in 0..segments {
            for i in 0..per {
                let (id, v) = &corpus.docs[s * per + i];
                h.insert(*id, v).unwrap();
            }
            h.commit_inner().unwrap();
        }
        // Update documents that are already committed: each one asks every
        // segment whether it holds the id.
        let sample: Vec<usize> = (0..500).map(|k| (k * 79) % (per * segments)).collect();
        let t = std::time::Instant::now();
        for &k in &sample {
            let (id, v) = &corpus.docs[k];
            h.insert(*id, v).unwrap();
        }
        let per_update = t.elapsed().as_secs_f64() * 1e6 / sample.len() as f64;
        eprintln!("{segments:>3} segments — update {per_update:>8.1} µs each");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
