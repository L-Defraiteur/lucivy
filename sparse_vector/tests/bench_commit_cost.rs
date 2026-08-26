//! What one sparse commit costs as the index grows — the measurement the
//! segment design rests on.
//!
//! `sparse.mmap` is rewritten whole at every commit, so **inserting one
//! vector costs the same as inserting the whole index**: 320 ms at 200 000
//! vectors on a 24-core machine, growing linearly with the file (26 / 97 /
//! 178 / 320 ms at 10k / 50k / 100k / 200k). At two million vectors that is
//! seconds, per single insert.
//!
//! Run: `cargo test --release -p sparse-vector --test bench_commit_cost -- --ignored --nocapture`
use sparse_vector::handle::SparseHandle;
use sparse_vector::index::SparseVector;

fn vec_of(seed: u64) -> SparseVector {
    let indices: Vec<u32> = (0..40).map(|k| ((seed * 2_654_435_761 + k * 97) % 50_000) as u32).collect();
    let values = vec![1.0f32; indices.len()];
    SparseVector { indices, values }
}

#[test]
#[ignore]
fn commit_cost_grows_with_the_index() {
    // Not asserted: this is a measurement, and a shared runner cannot time.
    let dir = std::env::temp_dir().join("lucivy_sparse_commit_cost");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let h = SparseHandle::create(dir.to_str().unwrap()).unwrap();
    for n in [10_000u64, 50_000, 100_000, 200_000] {
        let start = h.len() as u64;
        for i in start..n { h.insert(i, &vec_of(i)).unwrap(); }
        let t = std::time::Instant::now();
        h.commit_inner().unwrap();
        let bulk = t.elapsed().as_secs_f64() * 1e3;
        // one more vector, then commit again
        h.insert(n + 1, &vec_of(n + 1)).unwrap();
        let t = std::time::Instant::now();
        h.commit_inner().unwrap();
        let one = t.elapsed().as_secs_f64() * 1e3;
        let bytes = std::fs::metadata(dir.join("sparse.mmap")).unwrap().len();
        eprintln!("{n:>7} vectors — commit after bulk {bulk:>8.0} ms · commit after ONE more {one:>8.0} ms · sparse.mmap {:.1} MB",
            bytes as f64 / 1048576.0);
    }
    let _ = std::fs::remove_dir_all(&dir);
}
