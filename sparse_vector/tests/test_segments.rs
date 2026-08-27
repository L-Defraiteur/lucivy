//! An index is a list of segments, and it answers like one index.
//!
//! What is pinned here:
//!
//! - a commit writes **one segment for the delta** and leaves the previous
//!   ones alone (`tests/bench_commit_cost.rs` measures what that saves);
//! - searching N segments gives what one index holding everything gives —
//!   same documents, same scores, same order;
//! - an update lands the right way round: the copy in an older segment is
//!   hidden, the new one answers, and the count does not drift;
//! - a deletion survives a reopen, and so does everything else.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use sparse_vector::handle::SparseHandle;
use sparse_vector::index::{SparseIndex, SparseVector};

struct TempDir(PathBuf);
impl TempDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("lucivy_segments_{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
    fn str(&self) -> &str { self.0.to_str().unwrap() }
}
impl Drop for TempDir {
    fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
}

fn segment_count(base: &Path) -> usize {
    std::fs::read_dir(base).unwrap().flatten()
        .filter(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n.starts_with("seg_") && n.ends_with(".mmap")
        })
        .count()
}

/// Vectors whose dimensions are spread out, so a wrong dimension shows.
fn vector(id: u64, weight: f32) -> SparseVector {
    let indices: Vec<u32> = (0..6).map(|k| ((id * 7 + k * 1301) % 5_000) as u32).collect();
    SparseVector { indices, values: vec![weight; 6] }
}

fn queries() -> Vec<SparseVector> {
    (0..12u64).map(|i| vector(i * 13, 1.0)).collect()
}

/// The same documents in one in-RAM index: the reference every assertion
/// compares against.
fn reference(docs: &[(u64, SparseVector)]) -> SparseIndex {
    let mut index = SparseIndex::new();
    for (id, v) in docs { index.insert(*id, v); }
    index
}

#[test]
fn a_commit_writes_one_segment_and_the_answers_do_not_change() {
    let dir = TempDir::new("many_commits");
    let handle = SparseHandle::create(dir.str()).unwrap();

    let mut docs: Vec<(u64, SparseVector)> = Vec::new();
    for round in 0..5u64 {
        for i in 0..40u64 {
            let id = round * 40 + i;
            let v = vector(id, 1.0 + (id % 4) as f32);
            handle.insert(id, &v).unwrap();
            docs.push((id, v));
        }
        handle.commit_inner().unwrap();
        assert_eq!(segment_count(&dir.0), round as usize + 1,
            "each commit writes one segment, and leaves the earlier ones alone");
        assert_eq!(handle.len(), docs.len(), "after {} commits", round + 1);
    }

    // Five segments must answer exactly like one index holding everything.
    let reference = reference(&docs);
    for q in queries() {
        assert_eq!(handle.search(&q, 20), reference.search(&q, 20),
            "five segments disagree with one index");
    }
}

#[test]
fn an_update_hides_the_older_copy() {
    let dir = TempDir::new("update");
    let handle = SparseHandle::create(dir.str()).unwrap();

    for id in 0..30u64 { handle.insert(id, &vector(id, 1.0)).unwrap(); }
    handle.commit_inner().unwrap();

    // Re-insert a third of them with a different weight, in a new segment.
    let mut docs: Vec<(u64, SparseVector)> = (0..30u64).map(|id| (id, vector(id, 1.0))).collect();
    for id in (0..30u64).step_by(3) {
        let v = vector(id, 9.0);
        handle.insert(id, &v).unwrap();
        docs[id as usize] = (id, v);
    }
    handle.commit_inner().unwrap();

    assert_eq!(handle.len(), 30, "an update is not a second document");
    let reference = reference(&docs);
    for q in queries() {
        let got = handle.search(&q, 30);
        assert_eq!(got, reference.search(&q, 30), "the updated weight must be the one that answers");
        let ids: Vec<u64> = got.iter().map(|(id, _)| *id).collect();
        let mut unique = ids.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(ids.len(), unique.len(), "a document answered twice: {ids:?}");
    }
}

#[test]
fn a_deletion_holds_across_a_reopen() {
    let dir = TempDir::new("delete");
    let handle = SparseHandle::create(dir.str()).unwrap();
    for id in 0..50u64 { handle.insert(id, &vector(id, 1.0)).unwrap(); }
    handle.commit_inner().unwrap();

    let gone: Vec<u64> = (0..50u64).step_by(5).collect();
    for &id in &gone {
        assert!(handle.remove(id).unwrap(), "removing a committed document must report it");
    }
    assert!(!handle.remove(4242).unwrap(), "removing what is not there reports nothing");
    handle.commit_inner().unwrap();
    assert_eq!(handle.len(), 40);

    let kept: Vec<(u64, SparseVector)> = (0..50u64)
        .filter(|id| !gone.contains(id))
        .map(|id| (id, vector(id, 1.0)))
        .collect();
    let reference = reference(&kept);

    // Before and after a reopen, the deleted documents are gone and the
    // others are untouched.
    for handle in [handle, SparseHandle::open(dir.str()).unwrap()] {
        assert_eq!(handle.len(), 40);
        for q in queries() {
            let got = handle.search(&q, 50);
            assert!(got.iter().all(|(id, _)| !gone.contains(id)), "a deleted document answered");
            assert_eq!(got, reference.search(&q, 50));
        }
    }
}

#[test]
fn a_filtered_search_over_segments_is_the_search_intersected() {
    let dir = TempDir::new("filtered");
    let handle = SparseHandle::create(dir.str()).unwrap();
    let mut docs = Vec::new();
    for round in 0..3u64 {
        for i in 0..25u64 {
            let id = round * 25 + i;
            let v = vector(id, 1.0 + (id % 3) as f32);
            handle.insert(id, &v).unwrap();
            docs.push((id, v));
        }
        handle.commit_inner().unwrap();
    }
    let allowed: Vec<u64> = (0..75u64).step_by(4).collect();
    for q in queries() {
        let full: HashMap<u64, f32> = handle.search(&q, 75).into_iter().collect();
        let filtered = handle.search_filtered(&q, 75, &allowed);
        let mut expected: Vec<(u64, f32)> = full.into_iter()
            .filter(|(id, _)| allowed.contains(id))
            .collect();
        expected.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap().then(a.0.cmp(&b.0)));
        assert_eq!(filtered, expected);
    }
}

/// An index written before segments opens, answers, and its next commit
/// converts it — the whole-file write it used to do at every commit.
#[test]
fn an_index_from_before_segments_is_converted_by_its_next_commit() {
    let dir = TempDir::new("convert");
    let docs: Vec<(u64, SparseVector)> = (0..60u64).map(|id| (id, vector(id, 1.0 + (id % 5) as f32))).collect();

    // Build one, then take it back to the single-file layout by hand: what
    // 3.0.5 left on disk.
    {
        let handle = SparseHandle::create(dir.str()).unwrap();
        for (id, v) in &docs { handle.insert(*id, v).unwrap(); }
        handle.commit_inner().unwrap();
    }
    let seg: PathBuf = std::fs::read_dir(&dir.0).unwrap().flatten()
        .map(|e| e.path())
        .find(|p| p.file_name().unwrap().to_string_lossy().ends_with(".mmap"))
        .expect("a segment was written");
    // A single-file index is that segment under its old name, with the
    // manifest gone. Its dimension table is v3, which reads the same.
    std::fs::rename(&seg, dir.0.join("sparse.mmap")).unwrap();
    let _ = std::fs::remove_file(dir.0.join("meta.json"));
    for e in std::fs::read_dir(&dir.0).unwrap().flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        if n.ends_with(".ids") { let _ = std::fs::remove_file(e.path()); }
    }
    // No side files at all: a version 3 file names its own dimensions, and
    // the ids of the segment it becomes are read from its posting lists.

    let handle = SparseHandle::open(dir.str()).unwrap();
    assert_eq!(handle.len(), 60, "the old layout still opens");
    let before = reference(&docs);
    for q in queries() {
        assert_eq!(handle.search(&q, 20), before.search(&q, 20), "before conversion");
    }

    // The next commit converts it, and nothing of the old layout is left.
    handle.insert(999, &vector(999, 3.0)).unwrap();
    handle.commit_inner().unwrap();
    assert!(!dir.0.join("sparse.mmap").exists());
    assert!(!dir.0.join("sparse_dims.bin").exists());
    assert!(!dir.0.join("sparse_vectors.bin").exists());
    assert!(dir.0.join("meta.json").exists());
    assert_eq!(handle.len(), 61);

    let mut with_extra = docs.clone();
    with_extra.push((999, vector(999, 3.0)));
    let after = reference(&with_extra);
    for q in queries() {
        assert_eq!(handle.search(&q, 20), after.search(&q, 20), "after conversion");
    }
}

/// A merge is a walk over sorted token tables: nothing is remapped, the
/// answers do not move, the tombstones are applied and their bytes come
/// back. It is also the operation that would absorb another index's
/// segments — the same call, with those segments as input.
#[test]
fn a_merge_keeps_the_answers_and_applies_the_tombstones() {
    let dir = TempDir::new("merge");
    let handle = SparseHandle::create(dir.str()).unwrap();

    // Four segments, an update spanning two of them, and some deletions.
    let mut docs: HashMap<u64, SparseVector> = HashMap::new();
    for round in 0..4u64 {
        for i in 0..30u64 {
            let id = round * 30 + i;
            let v = vector(id, 1.0 + (id % 4) as f32);
            handle.insert(id, &v).unwrap();
            docs.insert(id, v);
        }
        if round == 2 {
            for id in (0..30u64).step_by(6) {
                let v = vector(id, 8.0);
                handle.insert(id, &v).unwrap();
                docs.insert(id, v);
            }
        }
        handle.commit_inner().unwrap();
    }
    for id in (5..120u64).step_by(11) {
        handle.remove(id).unwrap();
        docs.remove(&id);
    }
    handle.commit_inner().unwrap();

    assert_eq!(handle.num_segments(), 4);
    let before: Vec<Vec<(u64, f32)>> = queries().iter().map(|q| handle.search(q, 40)).collect();
    let live = handle.len();
    assert_eq!(live, docs.len());
    let bytes_before: u64 = std::fs::read_dir(&dir.0).unwrap().flatten()
        .filter_map(|e| e.metadata().ok().map(|m| m.len())).sum();

    handle.compact().unwrap();

    assert_eq!(handle.num_segments(), 1, "a merge leaves one segment");
    assert_eq!(segment_count(&dir.0), 1, "and removes the files nothing names any more");
    assert_eq!(handle.len(), live, "a merge does not change what is in the index");
    let after: Vec<Vec<(u64, f32)>> = queries().iter().map(|q| handle.search(q, 40)).collect();
    assert_eq!(after, before, "a merge does not move an answer");

    // The deleted documents' bytes are gone, and so are their tombstones.
    let bytes_after: u64 = std::fs::read_dir(&dir.0).unwrap().flatten()
        .filter_map(|e| e.metadata().ok().map(|m| m.len())).sum();
    assert!(bytes_after < bytes_before, "{bytes_after} is not smaller than {bytes_before}");

    // And the same answers again after a reopen, from the merged file alone.
    let reopened = SparseHandle::open(dir.str()).unwrap();
    assert_eq!(reopened.len(), live);
    let reference: Vec<(u64, SparseVector)> = docs.into_iter().collect();
    let reference = reference_index(&reference);
    for (q, want) in queries().iter().zip(&before) {
        assert_eq!(&reopened.search(q, 40), want);
        assert_eq!(reopened.search(q, 40), reference.search(q, 40));
    }
}

/// The reference builder under a name a local binding does not shadow.
fn reference_index(docs: &[(u64, SparseVector)]) -> SparseIndex {
    reference(docs)
}

/// Segments do not pile up on their own: past the cap, a commit merges.
#[test]
fn segments_are_merged_once_they_pile_up() {
    let dir = TempDir::new("policy");
    let handle = SparseHandle::create(dir.str()).unwrap();
    // The default the handle applies; the test follows it rather than
    // repeating a number that would drift out of step with it.
    let cap: usize = std::env::var("LUCIVY_SPARSE_MAX_SEGMENTS").ok()
        .and_then(|v| v.parse().ok()).unwrap_or(8);

    let mut docs: Vec<(u64, SparseVector)> = Vec::new();
    for round in 0..(cap as u64 + 4) {
        for i in 0..10u64 {
            let id = round * 10 + i;
            let v = vector(id, 1.0 + (id % 3) as f32);
            handle.insert(id, &v).unwrap();
            docs.push((id, v));
        }
        handle.commit_inner().unwrap();
        assert!(handle.num_segments() <= cap,
            "commit {round}: {} segments, cap is {cap}", handle.num_segments());
    }
    assert_eq!(handle.len(), docs.len());
    let want = reference(&docs);
    for q in queries() {
        assert_eq!(handle.search(&q, 30), want.search(&q, 30), "merging changed an answer");
    }
}

/// A filtered search does not depend on how the index happens to be laid
/// out: the same allowed set gives the same hits, the same scores and the
/// same order before and after a merge. Asked by the rag3weaver session
/// (doc 08, §2.1): can the filter be counted on *while* an index lives, and
/// not only after a compaction.
#[test]
fn a_filtered_search_is_the_same_before_and_after_a_merge() {
    // Six commits, under the cap: nothing merges until this test asks.
    // (Setting the environment variable here would reach the other tests —
    // the cap is read once per process.)
    let dir = TempDir::new("filter_merge");
    let handle = SparseHandle::create(dir.str()).unwrap();

    let mut docs: Vec<(u64, SparseVector)> = Vec::new();
    for round in 0..5u64 {
        for i in 0..24u64 {
            let id = round * 24 + i;
            let v = vector(id, 1.0 + (id % 4) as f32);
            handle.insert(id, &v).unwrap();
            docs.push((id, v));
        }
        handle.commit_inner().unwrap();
    }
    // Deletions too: a tombstone must not survive a merge as a missing hit.
    for id in (3..120u64).step_by(17) {
        handle.remove(id).unwrap();
        docs.retain(|(d, _)| *d != id);
    }
    handle.commit_inner().unwrap();
    assert!(handle.num_segments() >= 5, "the point is to search several segments");

    // Allowed sets from very selective to nearly everything.
    let sets: Vec<Vec<u64>> = vec![
        (0..120).step_by(37).collect(),        // ~3 ids
        (0..120).step_by(7).collect(),         // ~17
        (0..120).step_by(2).collect(),         // 60
        (0..120).collect(),                    // everything, including deleted ones
        vec![9_999, 10_000],                   // none of them exist
    ];

    let before: Vec<Vec<Vec<(u64, f32)>>> = sets.iter()
        .map(|ids| queries().iter().map(|q| handle.search_filtered(q, 40, ids)).collect())
        .collect();

    // And the reference: an in-RAM index holding exactly the live documents.
    let reference = reference(&docs);
    for (ids, per_query) in sets.iter().zip(&before) {
        for (q, got) in queries().iter().zip(per_query) {
            let mut want: Vec<(u64, f32)> = reference.search(q, 200).into_iter()
                .filter(|(id, _)| ids.contains(id))
                .collect();
            want.truncate(40);
            assert_eq!(got, &want, "a filtered search over segments is the search intersected");
        }
    }

    handle.compact().unwrap();
    assert_eq!(handle.num_segments(), 1);
    for (ids, per_query) in sets.iter().zip(&before) {
        for (q, want) in queries().iter().zip(per_query) {
            assert_eq!(&handle.search_filtered(q, 40, ids), want,
                "the merge moved a filtered answer");
        }
    }
}
