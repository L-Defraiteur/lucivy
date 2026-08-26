//! A sparse index says when its file is not the one that was written.
//!
//! Until 3.0.5 `sparse.mmap` was written straight onto the destination:
//! an interrupted commit (a crash, a full disk) truncated the index, which
//! then opened without complaining and answered from whatever was left.
//! Now the file is written to a temporary and renamed over the destination,
//! it carries a CRC-32 footer, and `open` checks the length its own headers
//! describe.
//!
//! What is pinned here: a truncated file is refused, a corrupted one is
//! caught by the checksum, a file written before this change still opens,
//! and a completed write leaves no temporary behind.

use std::path::{Path, PathBuf};

use sparse_vector::mmap_index::{write_mmap_file, MmapPostingData};
use sparse_vector::wand::Postings;

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("lucivy_sparse_durability_{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
    fn path(&self) -> &Path { &self.0 }
}

impl Drop for TempDir {
    fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
}

fn postings() -> Vec<Postings> {
    (0..8u32)
        .map(|d| Postings::from_pairs((0..50u64).map(|i| (i * 7 + d as u64, 1.0 + d as f32))))
        .collect()
}

fn write(dir: &TempDir) -> PathBuf {
    let path = dir.path().join("sparse.mmap");
    write_mmap_file(&path, &postings(), 400).unwrap();
    path
}

#[test]
fn a_complete_file_opens_and_verifies() {
    let dir = TempDir::new("complete");
    let path = write(&dir);
    let data = MmapPostingData::open(&path).unwrap();
    assert_eq!(data.num_dims(), 8);
    assert_eq!(data.num_vectors(), 400);
    assert_eq!(data.entries(0).len(), 50);
    data.verify_checksum(&path).unwrap();

    // The write went through a temporary, and cleaned up after itself.
    let leftovers: Vec<_> = std::fs::read_dir(dir.path()).unwrap()
        .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
        .filter(|n| n.ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "a temporary was left behind: {leftovers:?}");

    // Writing again over the same path keeps a file that opens.
    write_mmap_file(&path, &postings(), 400).unwrap();
    MmapPostingData::open(&path).unwrap().verify_checksum(&path).unwrap();
}

#[test]
fn a_truncated_file_is_refused() {
    let dir = TempDir::new("truncated");
    let path = write(&dir);
    let full = std::fs::read(&path).unwrap();

    // Every cut, from "the headers do not fit" to "one entry short".
    for cut in [8usize, 20, 100, full.len() / 2, full.len() - 9, full.len() - 1] {
        std::fs::write(&path, &full[..cut]).unwrap();
        let err = match MmapPostingData::open(&path) {
            Err(e) => e,
            Ok(_) => panic!("a file cut at {cut} of {} bytes must be refused", full.len()),
        };
        assert!(err.contains("truncated") || err.contains("too small"),
            "cut at {cut}: unhelpful message {err:?}");
    }
}

#[test]
fn a_corrupted_byte_is_caught_by_the_checksum() {
    let dir = TempDir::new("corrupt");
    let path = write(&dir);
    let mut data = std::fs::read(&path).unwrap();

    // A weight, deep in the entries: the length still matches, so only the
    // checksum can tell. This is the bit rot the footer is for.
    let victim = data.len() / 2;
    data[victim] ^= 0xff;
    std::fs::write(&path, &data).unwrap();

    // The length check passes — that is the point of the checksum.
    let opened = MmapPostingData::open(&path).unwrap();
    let err = opened.verify_checksum(&path).expect_err("a flipped byte must be caught");
    assert!(err.contains("checksum mismatch"), "unhelpful message: {err:?}");

    // And an open that asks for it refuses the file outright.
    std::env::set_var("LUCIVY_SPARSE_VERIFY_CRC", "1");
    let opened = MmapPostingData::open(&path);
    std::env::remove_var("LUCIVY_SPARSE_VERIFY_CRC");
    let err = opened.err().expect("LUCIVY_SPARSE_VERIFY_CRC must refuse it");
    assert!(err.contains("checksum mismatch"), "unhelpful message: {err:?}");
}

#[test]
fn a_file_written_before_this_change_still_opens() {
    let dir = TempDir::new("v1");
    let path = write(&dir);
    let data = std::fs::read(&path).unwrap();

    // Version 1: the footer never existed, and the header says 1.
    let mut v1 = data[..data.len() - 8].to_vec();
    v1[4..8].copy_from_slice(&1u32.to_le_bytes());
    std::fs::write(&path, &v1).unwrap();

    let opened = MmapPostingData::open(&path).unwrap();
    assert_eq!(opened.num_dims(), 8);
    assert_eq!(opened.entries(3).len(), 50);
    // No footer to check: verification passes rather than inventing a failure.
    opened.verify_checksum(&path).unwrap();

    // A version this build does not know is refused, with the range it reads.
    let mut future = v1.clone();
    future[4..8].copy_from_slice(&99u32.to_le_bytes());
    std::fs::write(&path, &future).unwrap();
    let err = MmapPostingData::open(&path).err().expect("an unknown version must be refused");
    assert!(err.contains("unsupported version: 99"), "unhelpful message: {err:?}");
}
