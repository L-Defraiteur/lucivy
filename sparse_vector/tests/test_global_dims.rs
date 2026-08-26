//! A dimension is its global token id (format version 3).
//!
//! The header table used to be addressed by position — a dense index local
//! to the file, which only `sparse_dims.bin` could translate back. That is
//! what made merging two files impossible without remapping, and what made
//! the position of a dimension mean something different in every file.
//!
//! What is pinned here: a file written today answers by token id, a search
//! gives the same hits whatever the token ids look like (they are not the
//! positions: the table is sorted), and — the trap this caught — reopening
//! an index and mutating it does not shuffle the dimensions, because the
//! postings are reloaded by token and no longer by position.

use std::path::PathBuf;

use sparse_vector::handle::SparseHandle;
use sparse_vector::index::{SparseIndex, SparseVector};
use sparse_vector::mmap_index::{write_mmap_file, MmapPostingData};
use sparse_vector::wand::Postings;

struct TempDir(PathBuf);
impl TempDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("lucivy_global_dims_{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}
impl Drop for TempDir {
    fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
}

/// Token ids far from their positions, and not in order: the file must sort
/// them, and every lookup must go through the token.
const TOKENS: [u32; 5] = [90_000, 12, 7_777, 3, 40_400];

fn vector(tokens: &[u32], weight: f32) -> SparseVector {
    SparseVector { indices: tokens.to_vec(), values: vec![weight; tokens.len()] }
}

#[test]
fn a_file_answers_by_token_id_whatever_the_order() {
    let dir = TempDir::new("by_token");
    let path = dir.0.join("seg.mmap");
    let postings: Vec<Postings> = (0..5)
        .map(|d| Postings::from_pairs((0..10u64).map(|i| (i * 5 + d as u64, 1.0 + d as f32))))
        .collect();
    write_mmap_file(&path, &postings, &TOKENS, 50).unwrap();

    let data = MmapPostingData::open(&path).unwrap();
    assert!(data.has_global_dims());
    assert_eq!(data.version(), 3);

    // Every dimension is found by its token, whatever its position was.
    for (d, &token) in TOKENS.iter().enumerate() {
        let entries = data.entries_of_token(token);
        assert_eq!(entries.len(), 10, "token {token} lost its postings");
        assert_eq!(entries[0].weight, 1.0 + d as f32);
    }
    // A token the file does not hold is empty, not a panic and not someone
    // else's postings.
    assert!(data.entries_of_token(4_242).is_empty());
    assert_eq!(data.dim_of_token(4_242), None);

    // The table is sorted, which is what makes a merge a merge-join.
    let tokens: Vec<u32> = data.tokens().map(|(t, _)| t).collect();
    let mut sorted = TOKENS.to_vec();
    sorted.sort_unstable();
    assert_eq!(tokens, sorted);
}

#[test]
fn an_empty_dimension_is_not_written() {
    let dir = TempDir::new("empty_dims");
    let path = dir.0.join("seg.mmap");
    let mut postings: Vec<Postings> = (0..5).map(|_| Postings::new()).collect();
    postings[2] = Postings::from_pairs([(1u64, 1.0f32), (2, 2.0)]);
    write_mmap_file(&path, &postings, &TOKENS, 2).unwrap();

    let data = MmapPostingData::open(&path).unwrap();
    assert_eq!(data.num_dims(), 1, "only the dimension that has postings is written");
    assert_eq!(data.entries_of_token(TOKENS[2]).len(), 2);
    assert!(data.entries_of_token(TOKENS[0]).is_empty());
}

/// The trap: after a reopen, a mutation reloads the postings into RAM. They
/// used to be read by position, which under a sorted table means every
/// dimension gets someone else's list — and a search then answers wrong
/// without failing.
#[test]
fn reopening_and_mutating_keeps_every_dimension_where_it_belongs() {
    let dir = TempDir::new("reopen");
    let base = dir.0.join("index");
    std::fs::create_dir_all(&base).unwrap();

    // Documents whose tokens are far apart, so a wrong mapping shows.
    let docs: Vec<(u64, SparseVector)> = (0..40u64)
        .map(|i| (i, vector(&[TOKENS[(i % 5) as usize], TOKENS[((i + 2) % 5) as usize]], 1.0 + (i % 3) as f32)))
        .collect();

    let h = SparseHandle::create(base.to_str().unwrap()).unwrap();
    for (id, v) in &docs { h.insert(*id, v).unwrap(); }
    h.commit_inner().unwrap();
    drop(h);

    // Reopen, insert one more document, search.
    let h = SparseHandle::open(base.to_str().unwrap()).unwrap();
    let extra = (999u64, vector(&[TOKENS[1]], 5.0));
    h.insert(extra.0, &extra.1).unwrap();
    h.commit_inner().unwrap();

    // The same content in one fresh in-RAM index is the reference.
    let mut reference = SparseIndex::new();
    for (id, v) in docs.iter().chain(std::iter::once(&extra)) { reference.insert(*id, v); }

    for &token in &TOKENS {
        let query = vector(&[token], 1.0);
        let got = h.search(&query, 50);
        let want = reference.search(&query, 50);
        assert_eq!(got, want, "token {token}: reopened index disagrees with a fresh one");
    }
    // And the document inserted after the reopen is found under its token.
    let hits = h.search(&vector(&[TOKENS[1]], 1.0), 50);
    assert!(hits.iter().any(|(id, _)| *id == 999), "the document added after reopening is missing");
}

/// A version 2 file — dense table, `sparse_dims.bin` for the translation —
/// still opens and still answers.
#[test]
fn a_dense_file_still_reads() {
    let dir = TempDir::new("dense");
    let path = dir.0.join("seg.mmap");
    let postings: Vec<Postings> = (0..3)
        .map(|d| Postings::from_pairs((0..6u64).map(|i| (i * 3 + d as u64, 1.0 + d as f32))))
        .collect();
    write_mmap_file(&path, &postings, &[10, 20, 30], 18).unwrap();

    // Rewrite the header as version 2 (the footer stays, it is version 2's too).
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[4..8].copy_from_slice(&2u32.to_le_bytes());
    std::fs::write(&path, &bytes).unwrap();

    let data = MmapPostingData::open(&path).unwrap();
    assert!(!data.has_global_dims(), "a version 2 table says nothing about tokens");
    assert_eq!(data.dim_of_token(10), None);
    // Read by position, which is what a dense file means.
    assert_eq!(data.entries(0).len(), 6);
    assert_eq!(data.num_dims(), 3);
}
