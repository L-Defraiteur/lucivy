//! What would a denser `.sfxpost` cost, on real data?
//!
//! Same protocol as `test_wsp_density`, which sized WSP3 before it was
//! written. `.sfxpost` is 585 MB of a 3.7 GB index. Per ordinal and per
//! document it stores 10 fixed bytes — `doc_id` (u32), `payload_offset`
//! (u32), `entry_count` (u16) — then a payload of `(token_index, byte_from,
//! byte_to)` triples that are already varints but **absolute**.
//!
//! Three things are measured, because they are three separate decisions:
//!   1. the per-document header in delta varints,
//!   2. the payload with the triple delta-encoded,
//!   3. the checkpoints that `find_doc`'s binary search would need to survive.
//!
//!   SFXPOST_DIR=/tmp/lucivy_parity_native \
//!   cargo test --release -p lucivy-core --test test_sfxpost_density -- --ignored --nocapture

use std::path::{Path, PathBuf};

fn varint_len(mut v: u64) -> usize {
    let mut n = 1;
    v >>= 7;
    while v > 0 {
        n += 1;
        v >>= 7;
    }
    n
}

/// Reads one of the writer's vints (`super::collector::encode_vint`).
fn read_vint(data: &[u8], pos: &mut usize) -> Option<u32> {
    let mut v: u32 = 0;
    for shift in [0u32, 7, 14, 21, 28] {
        let b = *data.get(*pos)?;
        *pos += 1;
        v |= ((b & 0x7f) as u32).checked_shl(shift)?;
        if b & 0x80 == 0 {
            return Some(v);
        }
    }
    None
}

const CHECKPOINT_EVERY: usize = 32;

#[derive(Default)]
struct Stats {
    files: usize,
    ordinals: u64,
    empty: u64,
    docs: u64,
    entries: u64,
    /// What the file holds today, entry_data only (no offset table).
    now_header: u64,
    now_payload: u64,
    /// Delta-varint header: d_doc, payload length, entry_count.
    new_header: u64,
    /// Payload with (token_index, byte_from) delta'd in the doc and
    /// byte_to stored as a length.
    new_payload: u64,
    /// 12 bytes per checkpoint (doc_id, payload offset, entry index).
    checkpoints: u64,
}

#[test]
#[ignore]
fn sfxpost_density() {
    let dir = std::env::var("SFXPOST_DIR").expect("SFXPOST_DIR=<index dir>");
    let max_files: usize = std::env::var("SFXPOST_MAX_FILES").ok().and_then(|v| v.parse().ok()).unwrap_or(4);

    let mut files = Vec::new();
    collect(Path::new(&dir), "sfxpost", &mut files);
    files.sort();
    eprintln!("[sfxpost] {} files under {dir}", files.len());

    let mut s = Stats::default();
    for path in files.iter().take(max_files) {
        let data = std::fs::read(path).unwrap();
        if data.len() < 8 || &data[0..4] != b"SFP2" {
            continue;
        }
        s.files += 1;
        let num_terms = u32::from_le_bytes(data[4..8].try_into().unwrap()) as usize;
        let table = 8;
        let entry_data_start = table + (num_terms + 1) * 4;
        let off = |i: usize| {
            u32::from_le_bytes(data[table + i * 4..table + i * 4 + 4].try_into().unwrap()) as usize
        };

        for ord in 0..num_terms {
            let (start, end) = (entry_data_start + off(ord), entry_data_start + off(ord + 1));
            s.ordinals += 1;
            if start >= end || end > data.len() {
                s.empty += 1;
                continue;
            }
            let n_docs = u32::from_le_bytes(data[start..start + 4].try_into().unwrap()) as usize;
            if n_docs == 0 {
                s.empty += 1;
                continue;
            }
            let ids = start + 4;
            let offs = ids + n_docs * 4;
            let counts = offs + n_docs * 4;
            let payload = counts + n_docs * 2;
            if payload > data.len() {
                s.empty += 1;
                continue;
            }
            s.docs += n_docs as u64;
            s.now_header += 4 + (n_docs * 10) as u64;
            s.new_header += varint_len(n_docs as u64) as u64;
            s.checkpoints += (n_docs.saturating_sub(1) / CHECKPOINT_EVERY * 12) as u64;

            let mut prev_doc = 0u32;
            for d in 0..n_docs {
                let doc = u32::from_le_bytes(data[ids + d * 4..ids + d * 4 + 4].try_into().unwrap());
                let p0 = u32::from_le_bytes(data[offs + d * 4..offs + d * 4 + 4].try_into().unwrap()) as usize;
                let p1 = if d + 1 < n_docs {
                    u32::from_le_bytes(data[offs + (d + 1) * 4..offs + (d + 1) * 4 + 4].try_into().unwrap()) as usize
                } else {
                    end - payload
                };
                let count = u16::from_le_bytes(data[counts + d * 2..counts + d * 2 + 2].try_into().unwrap()) as usize;
                s.entries += count as u64;
                s.now_payload += (p1 - p0) as u64;
                // The offset table becomes a per-document payload length, which
                // the checkpoints make seekable again.
                s.new_header += varint_len(doc.wrapping_sub(prev_doc) as u64) as u64
                    + varint_len((p1 - p0) as u64) as u64
                    + varint_len(count as u64) as u64;
                prev_doc = doc;

                let mut pos = payload + p0;
                let (mut prev_ti, mut prev_bf) = (0u32, 0u32);
                for _ in 0..count {
                    let (Some(ti), Some(bf), Some(bt)) = (
                        read_vint(&data, &mut pos), read_vint(&data, &mut pos), read_vint(&data, &mut pos),
                    ) else { break };
                    s.new_payload += varint_len(ti.wrapping_sub(prev_ti) as u64) as u64
                        + varint_len(bf.wrapping_sub(prev_bf) as u64) as u64
                        + varint_len(bt.wrapping_sub(bf) as u64) as u64;
                    prev_ti = ti;
                    prev_bf = bf;
                }
            }
        }
        eprintln!("[sfxpost] {}: {:.1} MB", path.file_name().unwrap().to_string_lossy(), data.len() as f64 / 1e6);
    }

    let now = s.now_header + s.now_payload;
    let new = s.new_header + s.new_payload + s.checkpoints;
    eprintln!(
        "\n[sfxpost] {} files, {} ordinals ({:.1} % empty), {} docs, {} entries",
        s.files, s.ordinals, s.empty as f64 * 100.0 / s.ordinals.max(1) as f64, s.docs, s.entries
    );
    eprintln!(
        "[sfxpost] header : {:8.1} MB -> {:8.1} MB ({:.1} B/doc -> {:.1} B/doc)",
        s.now_header as f64 / 1e6, s.new_header as f64 / 1e6,
        s.now_header as f64 / s.docs.max(1) as f64, s.new_header as f64 / s.docs.max(1) as f64,
    );
    eprintln!(
        "[sfxpost] payload: {:8.1} MB -> {:8.1} MB ({:.2} B/entry -> {:.2} B/entry)",
        s.now_payload as f64 / 1e6, s.new_payload as f64 / 1e6,
        s.now_payload as f64 / s.entries.max(1) as f64, s.new_payload as f64 / s.entries.max(1) as f64,
    );
    eprintln!(
        "[sfxpost] checkpoints (every {CHECKPOINT_EVERY} docs): {:.1} MB",
        s.checkpoints as f64 / 1e6
    );
    eprintln!(
        "[sfxpost] total  : {:8.1} MB -> {:8.1} MB ({:.2}x smaller)",
        now as f64 / 1e6, new as f64 / 1e6, now as f64 / new.max(1) as f64,
    );
    eprintln!(
        "[sfxpost] offset table: {:.1} MB ({} ordinals x 4 B) — a separate target",
        s.ordinals as f64 * 4.0 / 1e6, s.ordinals
    );
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
