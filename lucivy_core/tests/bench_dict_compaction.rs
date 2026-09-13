//! The compaction of a shard dictionary's generations, alone, on files of a
//! real index: what its two passes cost and where.
//!
//!   DICT_DIR=<dir holding dict-<g>.<field>.sfx/.termtexts> DICT_GENS=2,4,6,11 DICT_FIELD=2 \
//!   V3_PROFILE=1 cargo test --release -p lucivy-core --test bench_dict_compaction -- --ignored --nocapture
//!
//! The output generation (`DICT_OUT`, default 99) is written into `DICT_DIR`
//! and removed afterwards. Symlinks to the generation files of an index
//! are enough (`compact_generations` only reads them).
use std::time::Instant;

use ld_lucivy::directory::MmapDirectory;
use ld_lucivy::suffix_fst::dictionary::dictionary_file_name;
use ld_lucivy::suffix_fst::dictionary_compact::compact_generations;

#[test]
#[ignore]
fn bench_dict_compaction() {
    let dir = std::env::var("DICT_DIR").expect("DICT_DIR");
    let gens: Vec<u64> = std::env::var("DICT_GENS").unwrap_or_else(|_| "2,4,6,11".into())
        .split(',').map(|g| g.trim().parse().unwrap()).collect();
    let field: u32 = std::env::var("DICT_FIELD").ok().and_then(|v| v.parse().ok()).unwrap_or(2);
    let out: u64 = std::env::var("DICT_OUT").ok().and_then(|v| v.parse().ok()).unwrap_or(99);
    let rounds: usize = std::env::var("BENCH_ROUNDS").ok().and_then(|v| v.parse().ok()).unwrap_or(2);
    let directory = MmapDirectory::open(&dir).unwrap();
    let mut input = 0u64;
    for &g in &gens {
        for ext in ["sfx", "termtexts"] {
            let p = std::path::Path::new(&dir).join(dictionary_file_name(g, field, ext));
            input += std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
        }
    }
    eprintln!("compacting generations {gens:?} of field {field}: {:.0} MB of input", input as f64 / 1e6);
    for round in 0..rounds {
        for ext in ["sfx", "termtexts"] {
            let _ = std::fs::remove_file(std::path::Path::new(&dir).join(dictionary_file_name(out, field, ext)));
        }
        let t = Instant::now();
        let report = compact_generations(&directory, &gens, field, out).unwrap();
        eprintln!("round {round}: {:.2} s total | fst pass {:.2} s | texts pass {:.2} s | {} keys ({} merged), {} texts, .sfx {:.0} MB, .termtexts {:.0} MB",
            t.elapsed().as_secs_f64(), report.fst_wall.as_secs_f64(), report.texts_wall.as_secs_f64(),
            report.keys, report.keys_merged, report.texts, report.sfx_bytes as f64 / 1e6, report.termtexts_bytes as f64 / 1e6);
    }
    // `DICT_KEEP=1` leaves the output in place (to compare two builds).
    if std::env::var("DICT_KEEP").is_err() {
        for ext in ["sfx", "termtexts"] {
            let _ = std::fs::remove_file(std::path::Path::new(&dir).join(dictionary_file_name(out, field, ext)));
        }
    }
}
