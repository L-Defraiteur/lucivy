//! A filtered sparse search is the unfiltered one intersected with the
//! allowed set — same hits, same scores, same order.
//!
//! Asked by the rag3weaver session (doc 08, §1): a sparse score is a plain
//! dot product with no corpus statistics, so restricting the set cannot
//! change what a surviving document scores. It can only remove lines. That
//! is worth asserting rather than reasoning about, because two code paths
//! answer a filtered search — a binary search per lane for a small set, a
//! walk with a membership test for a large one — and they must agree with
//! each other and with the unfiltered search.
//!
//! It also pins the shape the ids may take: sorted or not, with duplicates,
//! with ids the index has never seen.
//!
//! **On the scores**: the documents and their order are identical, and the
//! scores agree to a few units in the last place — not bit for bit. The two
//! paths accumulate a document's lanes in a different order, and floating
//! addition is not associative: 0.043053508 against 0.04305351 for the same
//! document. Compare scores with a tolerance, never with `==`, across paths.

mod corpus_vectors;

use sparse_vector::handle::SparseHandle;
use sparse_vector::index::{SparseIndex, SparseVector};

/// Equal up to the order the lanes were added in (see the module docs).
fn close(a: f32, b: f32) -> bool {
    (a - b).abs() <= 1e-6 * a.abs().max(b.abs()).max(1e-6)
}

fn build(dir: &std::path::Path, docs: &[(u64, SparseVector)]) -> SparseHandle {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).unwrap();
    let h = SparseHandle::create(dir.to_str().unwrap()).unwrap();
    for (id, v) in docs { h.insert(*id, v).unwrap(); }
    h.commit_inner().unwrap();
    h
}

#[test]
fn filtered_equals_unfiltered_intersected_whatever_the_path() {
    // Real vectors when the dump is there, the committed 500-document
    // fixture otherwise — either way a shape a model actually produces.
    let corpus = corpus_vectors::from_dump(2_000)
        .unwrap_or_else(|| corpus_vectors::build(2_000, 30));
    let dir = std::env::temp_dir().join("lucivy_sparse_filter_truth");
    let h = build(&dir, &corpus.docs);

    let mut reference = SparseIndex::new();
    for (id, v) in &corpus.docs { reference.insert(*id, v); }

    let ids: Vec<u64> = corpus.docs.iter().map(|(id, _)| *id).collect();
    let sets: Vec<(&str, Vec<u64>)> = vec![
        ("3 ids (the seek path)", ids.iter().copied().step_by(ids.len() / 3).collect()),
        ("1 %", ids.iter().copied().step_by(100).collect()),
        ("50 %", ids.iter().copied().step_by(2).collect()),
        ("everything (the walk path)", ids.clone()),
        ("unsorted, with duplicates", {
            let mut v: Vec<u64> = ids.iter().copied().step_by(7).collect();
            v.extend(v.clone());
            v.reverse();
            v
        }),
        ("ids the index has never seen", vec![9_000_001, 9_000_002, 9_000_003]),
        ("half real, half unknown", ids.iter().copied().step_by(11)
            .chain((0..500u64).map(|i| 8_000_000 + i)).collect()),
    ];

    for (label, allowed) in &sets {
        let mut wanted: Vec<u64> = allowed.clone();
        wanted.sort_unstable();
        wanted.dedup();
        for q in corpus.queries.iter().take(30) {
            // The reference: everything, then keep what is allowed.
            let mut expected: Vec<(u64, f32)> = reference.search(q, corpus.docs.len())
                .into_iter()
                .filter(|(id, _)| wanted.binary_search(id).is_ok())
                .collect();
            expected.truncate(20);
            let got = h.search_filtered(q, 20, allowed);
            assert_eq!(got.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
                       expected.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
                "{label}: a filtered search must be the unfiltered one intersected, in order");
            for ((id, got), (_, want)) in got.iter().zip(&expected) {
                assert!(close(*got, *want), "{label}: doc {id} scores {got}, unfiltered {want}");
            }
        }
    }

    // And the scores are the unfiltered ones, not rescaled: a dot product
    // has no corpus statistics to shift (which is what makes this different
    // from the BM25 side, where a filtered search rescores on the subset).
    let allowed: Vec<u64> = ids.iter().copied().step_by(9).collect();
    for q in corpus.queries.iter().take(20) {
        let full: std::collections::HashMap<u64, f32> =
            h.search(q, corpus.docs.len()).into_iter().collect();
        for (id, score) in h.search_filtered(q, 50, &allowed) {
            let unfiltered = *full.get(&id).expect("a filtered hit must exist unfiltered");
            assert!(close(score, unfiltered),
                "the filter changed a score: doc {id} scores {score}, unfiltered {unfiltered}");
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}
