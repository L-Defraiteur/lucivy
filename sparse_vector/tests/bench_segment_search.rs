//! What a search pays per segment, and what a merge gives back.
//!
//! A commit appends a segment, so an index that is written often ends up
//! with many; each one is walked per query dimension, and the WAND pruning
//! happens inside it rather than over the whole index. This is the number a
//! compaction policy is set from.
//!
//! Run: `cargo test --release -p sparse-vector --test bench_segment_search -- --ignored --nocapture`

use sparse_vector::handle::SparseHandle;
use sparse_vector::index::SparseVector;

fn vector(seed: u64) -> SparseVector {
    let indices: Vec<u32> = (0..40).map(|k| ((seed * 2_654_435_761 + k * 97) % 50_000) as u32).collect();
    let values = vec![1.0f32; indices.len()];
    SparseVector { indices, values }
}

#[test]
#[ignore]
fn search_cost_per_segment() {
    let dir = std::env::temp_dir().join("lucivy_sparse_segment_search");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let h = SparseHandle::create(dir.to_str().unwrap()).unwrap();

    const TOTAL: u64 = 100_000;
    let queries: Vec<SparseVector> = (0..50u64).map(|i| vector(i * 977 + 3)).collect();
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

    for segments in [1u64, 2, 5, 10, 20] {
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let h = SparseHandle::create(dir.to_str().unwrap()).unwrap();
        let per = TOTAL / segments;
        for s in 0..segments {
            for i in 0..per { h.insert(s * per + i, &vector(s * per + i)).unwrap(); }
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
