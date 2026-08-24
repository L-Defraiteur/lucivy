//! `StdFsDirectory` reads lazily: nothing at open, small reads straight from
//! the file, whole files through a bounded cache — and a file deleted while
//! a handle still references it stays readable through that handle (unlink
//! semantics, what a searcher holding merged-away segments relies on).

use ld_lucivy::directory::{Directory, TerminatingWrite};
use ld_lucivy::HasLen;
use lucivy_core::directory::StdFsDirectory;
use std::io::Write;
use std::path::Path;

fn write_file(dir: &StdFsDirectory, name: &str, data: &[u8]) {
    let mut w = dir.open_write(Path::new(name)).unwrap();
    w.write_all(data).unwrap();
    w.terminate().unwrap();
}

#[test]
fn lazy_handle_reads_ranges_and_whole_files() {
    let tmp = std::env::temp_dir().join("lucivy_lazy_dir_ranges");
    let _ = std::fs::remove_dir_all(&tmp);
    let dir = StdFsDirectory::open(&tmp).unwrap();
    let data: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    write_file(&dir, "big.bin", &data);

    let slice = dir.open_read(Path::new("big.bin")).unwrap();
    assert_eq!(slice.len(), data.len());
    // Footer-sized read: served from the file, no materialisation.
    let tail = slice.slice_from(data.len() - 16).read_bytes().unwrap();
    assert_eq!(tail.as_slice(), &data[data.len() - 16..]);
    // Whole read: materialised, then an Arc slice.
    let whole = slice.read_bytes().unwrap();
    assert_eq!(whole.as_slice(), &data[..]);
    let mid = slice.slice(1000..5000).read_bytes().unwrap();
    assert_eq!(mid.as_slice(), &data[1000..5000]);
}

#[test]
fn deleted_file_stays_readable_through_a_live_handle() {
    let tmp = std::env::temp_dir().join("lucivy_lazy_dir_unlink");
    let _ = std::fs::remove_dir_all(&tmp);
    let dir = StdFsDirectory::open(&tmp).unwrap();
    let data = b"segment bytes a merge is about to replace".to_vec();
    write_file(&dir, "seg.sfx", &data);

    // Opened but never read: the handle only knows the length.
    let stale = dir.open_read(Path::new("seg.sfx")).unwrap();
    assert_eq!(stale.len(), data.len());

    dir.delete(Path::new("seg.sfx")).unwrap();
    assert!(!tmp.join("seg.sfx").exists(), "the file is really gone from disk");

    // The reader that still holds the handle keeps working.
    assert_eq!(stale.read_bytes().unwrap().as_slice(), &data[..]);
    assert_eq!(stale.slice(8..13).read_bytes().unwrap().as_slice(), b"bytes");

    // A fresh open of the deleted name fails, as it should.
    assert!(dir.open_read(Path::new("seg.sfx")).is_err());
}

#[test]
fn rewritten_file_is_not_served_from_the_old_cache_entry() {
    let tmp = std::env::temp_dir().join("lucivy_lazy_dir_rewrite");
    let _ = std::fs::remove_dir_all(&tmp);
    let dir = StdFsDirectory::open(&tmp).unwrap();
    dir.atomic_write(Path::new("meta.json"), b"v1").unwrap();
    let h1 = dir.open_read(Path::new("meta.json")).unwrap();
    assert_eq!(h1.read_bytes().unwrap().as_slice(), b"v1");
    dir.atomic_write(Path::new("meta.json"), b"v2-longer").unwrap();
    let h2 = dir.open_read(Path::new("meta.json")).unwrap();
    assert_eq!(h2.read_bytes().unwrap().as_slice(), b"v2-longer");
    assert_eq!(dir.atomic_read(Path::new("meta.json")).unwrap(), b"v2-longer");
}
