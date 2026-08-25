//! What would a denser `.word_sfxpost` actually cost?
//!
//! The file is 17 % of an index and 78-82 % of it is touched by a common
//! query (`test_touched_bytes`), so it dominates the working set. It stores
//! five fixed u32 per entry. This reads real files and computes, on the real
//! data, what delta + varint encoding would give — and how many entries an
//! ordinal actually holds, since page granularity, not entry size, is what
//! costs on a scattered read.
//!
//!   WSP_DIR=/tmp/lucivy_parity_native \
//!   cargo test --release -p lucivy-core --test test_wsp_density -- --ignored --nocapture

use std::path::{Path, PathBuf};

/// Bytes a LEB128 varint takes.
fn varint_len(v: u64) -> usize {
    let mut n = 1;
    let mut v = v >> 7;
    while v > 0 {
        n += 1;
        v >>= 7;
    }
    n
}

#[derive(Default)]
struct Stats {
    files: usize,
    ordinals: u64,
    empty_ordinals: u64,
    entries: u64,
    current_bytes: u64,
    varint_bytes: u64,
    /// Variant B: byte_from delta'd within the document too (it is monotone
    /// there: entries of one doc are sorted by position, bytes follow).
    varint_b_bytes: u64,
    /// Variant C: B plus a skip checkpoint every 32 entries (12 B each) so
    /// `entry_at` keeps a binary search instead of scanning the ordinal.
    varint_c_bytes: u64,
    /// Entries per ordinal, bucketed: 1, 2-4, 5-16, 17-64, 65-256, >256.
    buckets: [u64; 6],
    /// Entries in each bucket.
    bucket_entries: [u64; 6],
}

fn bucket(n: usize) -> usize {
    match n {
        0..=1 => 0,
        2..=4 => 1,
        5..=16 => 2,
        17..=64 => 3,
        65..=256 => 4,
        _ => 5,
    }
}

#[test]
#[ignore]
fn word_sfxpost_density() {
    let dir = std::env::var("WSP_DIR").expect("WSP_DIR=<index dir>");
    let max_files: usize = std::env::var("WSP_MAX_FILES").ok().and_then(|v| v.parse().ok()).unwrap_or(8);

    let mut files = Vec::new();
    collect(Path::new(&dir), "word_sfxpost", &mut files);
    files.sort();
    eprintln!("[wsp] {} .word_sfxpost files under {dir}", files.len());

    let mut s = Stats::default();
    for path in files.iter().take(max_files) {
        let data = std::fs::read(path).unwrap();
        if data.len() < 8 || &data[0..4] != b"WSP2" {
            eprintln!("[wsp] {} — not WSP2, skipped", path.display());
            continue;
        }
        s.files += 1;
        let num_ords = u32::from_le_bytes(data[4..8].try_into().unwrap()) as usize;
        let table = 8;
        let read_off = |i: usize| u32::from_le_bytes(data[table + i * 4..table + i * 4 + 4].try_into().unwrap()) as usize;

        for ord in 0..num_ords {
            let (start, end) = (read_off(ord), read_off(ord + 1));
            let n = (end - start) / 20;
            s.ordinals += 1;
            if n == 0 {
                s.empty_ordinals += 1;
                continue;
            }
            s.entries += n as u64;
            s.current_bytes += (n * 20) as u64;
            let b = bucket(n);
            s.buckets[b] += 1;
            s.bucket_entries[b] += n as u64;
            // One checkpoint per 32 entries after the first block.
            s.varint_c_bytes += (n.saturating_sub(1) / 32 * 12) as u64;

            // Entries are sorted by (doc_id, first_position, last_position,
            // byte_from, byte_to). Delta the two monotone fields, and store
            // the others as the small quantities they are.
            let (mut prev_doc, mut prev_first, mut prev_from) = (0u32, 0u32, 0u32);
            for k in 0..n {
                let o = start + k * 20;
                let g = |j: usize| u32::from_le_bytes(data[o + j * 4..o + j * 4 + 4].try_into().unwrap());
                let (doc, first, last, from, to) = (g(0), g(1), g(2), g(3), g(4));
                let d_doc = doc.wrapping_sub(prev_doc);
                // first_position restarts at each new document.
                let d_first = if doc == prev_doc { first.wrapping_sub(prev_first) } else { first };
                let common = varint_len(d_doc as u64) as u64
                    + varint_len(d_first as u64) as u64
                    + varint_len(last.wrapping_sub(first) as u64) as u64
                    + varint_len(to.wrapping_sub(from) as u64) as u64;
                s.varint_bytes += common + varint_len(from as u64) as u64;
                let d_from = if doc == prev_doc { from.wrapping_sub(prev_from) } else { from };
                s.varint_b_bytes += common + varint_len(d_from as u64) as u64;
                prev_doc = doc;
                prev_first = first;
                prev_from = from;
            }
        }
        eprintln!(
            "[wsp] {}: {:.1} MB, {} ordinals ({} empty), {} entries",
            path.file_name().unwrap().to_string_lossy(),
            data.len() as f64 / 1e6,
            num_ords,
            s.empty_ordinals,
            s.entries
        );
    }

    let table_bytes = s.ordinals * 4;
    eprintln!(
        "\n[wsp] {} files, {} ordinals ({:.1} % empty), {} entries",
        s.files,
        s.ordinals,
        s.empty_ordinals as f64 * 100.0 / s.ordinals.max(1) as f64,
        s.entries
    );
    eprintln!(
        "[wsp] entries: {:.1} MB fixed (20 B) -> {:.1} MB varint ({:.2} B/entry, {:.2}x smaller)",
        s.current_bytes as f64 / 1e6,
        s.varint_bytes as f64 / 1e6,
        s.varint_bytes as f64 / s.entries.max(1) as f64,
        s.current_bytes as f64 / s.varint_bytes.max(1) as f64,
    );
    eprintln!(
        "[wsp] variant B (byte_from delta'd in the doc): {:.1} MB ({:.2} B/entry, {:.2}x)",
        s.varint_b_bytes as f64 / 1e6,
        s.varint_b_bytes as f64 / s.entries.max(1) as f64,
        s.current_bytes as f64 / s.varint_b_bytes.max(1) as f64,
    );
    let c = s.varint_b_bytes + s.varint_c_bytes;
    eprintln!(
        "[wsp] variant C (B + a skip checkpoint every 32 entries, keeps the binary search): {:.1} MB ({:.2} B/entry, {:.2}x) — checkpoints {:.1} MB",
        c as f64 / 1e6,
        c as f64 / s.entries.max(1) as f64,
        s.current_bytes as f64 / c.max(1) as f64,
        s.varint_c_bytes as f64 / 1e6,
    );
    eprintln!(
        "[wsp] offset table: {:.1} MB ({:.1} % of the file today)",
        table_bytes as f64 / 1e6,
        table_bytes as f64 * 100.0 / (table_bytes + s.current_bytes) as f64,
    );
    let names = ["1", "2-4", "5-16", "17-64", "65-256", ">256"];
    eprintln!("[wsp] entries per ordinal:");
    for i in 0..6 {
        if s.buckets[i] == 0 {
            continue;
        }
        eprintln!(
            "[wsp]   {:>6}: {:9} ordinals ({:5.1} %), {:11} entries ({:5.1} % of all), {:6.0} B today, {:6.0} B varint",
            names[i],
            s.buckets[i],
            s.buckets[i] as f64 * 100.0 / (s.ordinals - s.empty_ordinals).max(1) as f64,
            s.bucket_entries[i],
            s.bucket_entries[i] as f64 * 100.0 / s.entries.max(1) as f64,
            s.bucket_entries[i] as f64 * 20.0 / s.buckets[i] as f64,
            s.bucket_entries[i] as f64 * (s.varint_bytes as f64 / s.entries.max(1) as f64) / s.buckets[i] as f64,
        );
    }
}

fn collect(dir: &Path, ext: &str, out: &mut Vec<PathBuf>) {
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                collect(&p, ext, out);
            } else if p.extension().and_then(|x| x.to_str()) == Some(ext) {
                out.push(p);
            }
        }
    }
}
