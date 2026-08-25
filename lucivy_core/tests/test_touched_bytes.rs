//! How many bytes of an index does one query actually touch?
//!
//! The browser reads whole files; the native path mmaps them and only faults
//! the pages it reads. The ratio between the two is what a paged reader
//! (design `docs/25-08-2026/02-design-bytes-pagine.md`) would save.
//!
//! Protocol: open the index (merges settle), drop every page of every file
//! from the page cache (`posix_fadvise(DONTNEED)`), run one query, then read
//! `mincore` on each file to count resident pages. Eviction happens **after**
//! the open, so opening and merging do not pollute the measurement.
//!
//!   TOUCHED_INDEX=/tmp/lucivy_parity_native TOUCHED_QUERY=kmalloc \
//!   cargo test --release -p lucivy-core --test test_touched_bytes -- --ignored --nocapture

use lucivy_core::query::QueryConfig;
use lucivy_core::sharded_handle::ShardedHandle;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[test]
#[ignore]
fn touched_bytes_per_query() {
    let index = std::env::var("TOUCHED_INDEX").expect("TOUCHED_INDEX=<index dir>");
    let value = std::env::var("TOUCHED_QUERY").unwrap_or_else(|_| "kmalloc".into());
    let field = std::env::var("TOUCHED_FIELD").unwrap_or_else(|_| "content".into());
    let root = PathBuf::from(&index);

    let h = ShardedHandle::open(&index).unwrap();
    eprintln!("[touched] opened {} docs, {} shards", h.num_docs(), h.num_shards());

    let files = list_files(&root);
    let total: u64 = files.iter().map(|(_, len)| *len).sum();
    eprintln!("[touched] {} files, {:.1} MB on disk", files.len(), total as f64 / 1e6);
    // What the residency decision sees must match what is actually there: an
    // undercount would claim an index fits in memory when it does not.
    for i in 0..h.num_shards() {
        let (b, opened, listed) = h.shard_bytes_and_files(i);
        eprintln!("[touched] shard {i}: {:.1} MB, {opened} files opened of {listed} listed", b as f64 / 1e6);
    }
    // Which listed components have no file: those are what every count, every
    // GC pass and every snapshot walk pays for and never finds.
    {
        use std::collections::BTreeMap;
        let mut missing: BTreeMap<String, usize> = BTreeMap::new();
        let mut present: BTreeMap<String, usize> = BTreeMap::new();
        for i in 0..h.num_shards() {
            let Some(sh) = h.shard(i) else { continue };
            let Ok(metas) = sh.index.searchable_segment_metas() else { continue };
            let dir = sh.index.directory();
            let sfx_version = sh.index.settings().sfx_version;
            for p in metas.iter().flat_map(|m| m.list_files_for(sfx_version)) {
                let name = p.to_string_lossy();
                let ext = name.splitn(2, '.').nth(1).unwrap_or("").to_string();
                if ld_lucivy::Directory::open_read(dir, &p).is_ok() {
                    *present.entry(ext).or_insert(0) += 1;
                } else {
                    *missing.entry(ext).or_insert(0) += 1;
                }
            }
        }
        eprintln!("[touched] components PRESENT: {present:?}");
        eprintln!("[touched] components MISSING: {missing:?}");
        // A segment's meta must name what the segment carries and nothing else:
        // a phantom file costs an open on every walk, and it hides a file that
        // is genuinely unreachable behind one that was never meant to be there.
        assert!(missing.is_empty(), "list_files_for names files that do not exist: {missing:?}");
    }
    let counted = h.index_bytes() as f64 / 1e6;
    eprintln!("[touched] index_bytes() says {counted:.1} MB ({:.1} % of the directory), residency {:?}",
        counted * 100.0 / (total as f64 / 1e6), h.residency());

    // A first query so lazy structures (schema, tokenizers, segment metas) are
    // already in place: we want the cost of a query, not of the first one.
    let q = QueryConfig {
        query_type: "contains".into(),
        field: Some(field.clone()),
        value: Some(value.clone()),
        ..Default::default()
    };
    // Warm up with a *different* term: what we want is the marginal cost of a
    // query, not the one-off faulting of the structures every query shares.
    let warm_value = std::env::var("TOUCHED_WARM").unwrap_or_else(|_| "netdev".into());
    let warm = h.search(&QueryConfig { value: Some(warm_value.clone()), ..q.clone() }, 10, None).unwrap();
    eprintln!("[touched] warm-up query {warm_value:?}: {} hits", warm.len());

    for (p, _) in &files {
        evict(p);
    }
    let resident_before = resident_by_ext(&files);
    let before: u64 = resident_before.values().sum();
    eprintln!("[touched] resident after eviction: {:.1} MB", before as f64 / 1e6);
    let sizes0 = sizes_by_ext(&files);
    let mut left: Vec<(&String, &u64)> = resident_before.iter().filter(|(_, v)| **v > 0).collect();
    left.sort_by(|a, b| b.1.cmp(a.1));
    for (ext, bytes) in left.iter().take(10) {
        eprintln!("[touched]   still resident {ext:14} {:8.1} MB of {:8.1} MB",
            **bytes as f64 / 1e6, sizes0.get(*ext).copied().unwrap_or(0) as f64 / 1e6);
    }

    let t = std::time::Instant::now();
    let hits = h.search(&q, 10, None).unwrap();
    let ms = t.elapsed().as_secs_f64() * 1e3;

    let resident_after = resident_by_ext(&files);
    let after: u64 = resident_after.values().sum();
    eprintln!(
        "[touched] query {:?} on {field}: {} hits in {ms:.0}ms — touched {:.1} MB of {:.1} MB ({:.2} %)",
        value,
        hits.len(),
        (after - before) as f64 / 1e6,
        total as f64 / 1e6,
        (after - before) as f64 * 100.0 / total as f64,
    );

    let mut by_ext: Vec<(String, u64, u64)> = resident_after
        .iter()
        .map(|(ext, a)| (ext.clone(), a - resident_before.get(ext).copied().unwrap_or(0), *a))
        .collect();
    by_ext.sort_by(|a, b| b.1.cmp(&a.1));
    let sizes = sizes_by_ext(&files);
    for (ext, touched, _) in by_ext.iter().take(12) {
        if *touched == 0 {
            continue;
        }
        let size = sizes.get(ext).copied().unwrap_or(0);
        eprintln!(
            "[touched]   {ext:14} {:8.1} MB touched of {:8.1} MB ({:5.2} %)",
            *touched as f64 / 1e6,
            size as f64 / 1e6,
            *touched as f64 * 100.0 / size.max(1) as f64,
        );
    }
    h.close().unwrap();
}

fn list_files(dir: &Path) -> Vec<(PathBuf, u64)> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(list_files(&p));
            } else if let Ok(m) = e.metadata() {
                out.push((p, m.len()));
            }
        }
    }
    out
}

fn ext_of(p: &Path) -> String {
    p.extension().and_then(|e| e.to_str()).unwrap_or("(none)").to_string()
}

fn sizes_by_ext(files: &[(PathBuf, u64)]) -> HashMap<String, u64> {
    let mut m = HashMap::new();
    for (p, len) in files {
        *m.entry(ext_of(p)).or_insert(0) += len;
    }
    m
}

/// Drop this file's pages from the page cache.
fn evict(path: &Path) {
    use std::os::unix::io::AsRawFd;
    let Ok(f) = std::fs::File::open(path) else { return };
    unsafe {
        // POSIX_FADV_DONTNEED = 4
        libc::posix_fadvise(f.as_raw_fd(), 0, 0, 4);
    }
}

/// Resident bytes per extension, via mmap + mincore.
fn resident_by_ext(files: &[(PathBuf, u64)]) -> HashMap<String, u64> {
    use std::os::unix::io::AsRawFd;
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
    let mut out: HashMap<String, u64> = HashMap::new();
    for (path, len) in files {
        if *len == 0 {
            continue;
        }
        let Ok(f) = std::fs::File::open(path) else { continue };
        let len = *len as usize;
        let addr = unsafe {
            libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_SHARED, f.as_raw_fd(), 0)
        };
        if addr == libc::MAP_FAILED {
            continue;
        }
        let pages = len.div_ceil(page);
        let mut vec = vec![0u8; pages];
        let resident = unsafe {
            if libc::mincore(addr, len, vec.as_mut_ptr()) == 0 {
                vec.iter().filter(|b| *b & 1 == 1).count()
            } else {
                0
            }
        };
        unsafe { libc::munmap(addr, len) };
        *out.entry(ext_of(path)).or_insert(0) += (resident * page) as u64;
    }
    out
}
