//! What a search pays per segment, and what a merge gives back.
//!
//! **Nothing measurable.** Three runs on an idle machine, 40 000 documents,
//! 200 real BGE-M3 queries, search time against the same search after a
//! merge — and the same three runs on the text-derived corpus:
//!
//! | segments | 1 | 5 | 20 | 50 | 100 |
//! |---|---|---|---|---|---|
//! | BGE-M3 | ×1.0 | ×1.5 | ×1.3-1.4 | ×1.0-1.1 | ×1.2 |
//! | text, `tf · idf` | ×1.0 | ×1.3-1.4 | ×1.1-1.3 | ×1.0 | ×1.0-1.3 |
//!
//! Splitting the index splits the posting lists with it, and WAND prunes
//! inside each piece; a segment adds a binary search per query dimension.
//!
//! ## Two numbers this bench got wrong, and how
//!
//! It claimed ×5.3 on twenty segments, then ×7.8 on a hundred. Both were
//! reproducible when they were taken, and both were artefacts:
//!
//! 1. **The corpus.** The first version spread dimensions by hashing and
//!    gave every weight 1.0. Flat weights leave WAND nothing to prune with,
//!    so every segment restarts a full walk — it measured the generator.
//! 2. **The machine.** The second was taken on real vectors while the same
//!    machine was running the model that produced them. Under load, the
//!    many-segment cases inflate; on an idle machine the trend disappears
//!    and does not come back in three runs.
//!
//! So: a benchmark on synthetic data measures the generator, and a benchmark
//! on a busy machine measures the load. Neither announces itself.
//!
//! What does grow with the segment count, slightly, is the write path
//! (`update_cost_per_segment`): an insert or a delete asks every segment
//! whether it holds the id.
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

    // Real BGE-M3 vectors when the dump is there, text-derived ones
    // otherwise: WAND can only be measured on an unbalanced corpus.
    let want: usize = std::env::var("BENCH_DOCS").ok().and_then(|v| v.parse().ok()).unwrap_or(40_000);
    // `BENCH_CORPUS=text` compares against the text-derived generator.
    let corpus = match std::env::var("BENCH_CORPUS").as_deref() {
        Ok("text") => corpus_vectors::build(want, 200),
        _ => corpus_vectors::from_dump(want)
            .unwrap_or_else(|| corpus_vectors::build(want, 200)),
    };
    eprintln!("  corpus: {} (x{} replicas)", corpus_vectors::describe(&corpus),
        corpus_vectors::replicas(&corpus));
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
    let corpus = corpus_vectors::from_dump(40_000)
        .unwrap_or_else(|| corpus_vectors::build(40_000, 4));
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
