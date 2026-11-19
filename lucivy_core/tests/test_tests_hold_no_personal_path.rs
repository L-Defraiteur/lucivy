//! No test or bench of lucivy-core names a path of one machine, and no test passes by skipping (ticket of
//! 18 November 2026, « des tests de lucivy lisent des chemins personnels et passent en sautant »).
//!
//! Until then, four diagnostics and `bench_vs_tantivy` read indexes under the home directory of an old
//! workstation: on every other machine they found nothing, printed a line and passed. Two more tests printed
//! « Skipping » and passed when their data was missing. A test that skips is not green: a diagnostic on a
//! persisted index is `#[ignore]` and takes its path from the environment; a missing corpus goes through
//! `common::usable` (red with its recipe, or a skip said as such under `LUCIVY_CORPUS_ABSENT_OK=1`).

use std::path::Path;

fn sources(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    let mut entries: Vec<_> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        if p.is_dir() { sources(&p, out) } else if p.extension().is_some_and(|x| x == "rs") { out.push(p) }
    }
}

#[test]
fn tests_and_benches_name_no_personal_path_and_never_pass_by_skipping() {
    let core = Path::new(env!("CARGO_MANIFEST_DIR"));
    let me = core.join("tests").join(file!().rsplit('/').next().unwrap());
    // The patterns are built here so that this file does not match itself.
    let home = format!("\"/{}/", "home");
    let skips = [format!("\"{}", "Skipping"), format!("\"{}:", "SKIP"), format!("{}. ", "Skipping")];
    let mut found = Vec::new();
    for dir in ["tests", "benches"] {
        let mut files = Vec::new();
        sources(&core.join(dir), &mut files);
        for path in files {
            if path == me { continue; }
            let text = std::fs::read_to_string(&path).unwrap();
            let name = path.strip_prefix(core).unwrap().display().to_string();
            for (n, line) in text.lines().enumerate() {
                if line.contains(&home) {
                    found.push(format!("{name}:{}: a personal path: {}", n + 1, line.trim()));
                }
                // A skip message in a test file (not in benches, whose ignored runs report partial columns).
                if dir == "tests" && skips.iter().any(|s| line.contains(s.as_str())) {
                    found.push(format!("{name}:{}: a test that passes by skipping: {}", n + 1, line.trim()));
                }
            }
        }
    }
    assert!(found.is_empty(), "{} lines:\n{}", found.len(), found.join("\n"));
}
