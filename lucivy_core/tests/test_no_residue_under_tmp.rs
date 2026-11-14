//! No test writes under a fixed path in /tmp (14 November 2026): an index goes
//! to `common::scratch`, a report to `common::report_path`, and only the corpus
//! roots of `common::CORPUS_ROOTS` are read there. Checked on the sources: a
//! test that left a directory behind is found before it runs anywhere.

mod common;

/// Every `.rs` file under `dir`, recursively.
fn rust_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() { rust_files(&path, out); }
        else if path.extension().is_some_and(|e| e == "rs") { out.push(path); }
    }
}

#[test]
fn no_test_writes_under_a_fixed_tmp_path() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut offenders = Vec::new();
    let mut files = Vec::new();
    // tests/ and benches/ (the benches are [[test]] targets: they run under
    // `cargo test`), and the unit tests of src/ — whose production code may
    // use the temp dir (a default cache), but never a literal /tmp path.
    for sub in ["tests", "benches", "src"] { rust_files(&root.join(sub), &mut files); }
    files.sort();
    for path in files {
        let name = path.strip_prefix(root).unwrap().to_string_lossy().to_string();
        let in_src = name.starts_with("src");
        if name.ends_with("test_no_residue_under_tmp.rs") || name.ends_with("common/mod.rs") { continue; }
        let text = std::fs::read_to_string(&path).unwrap();
        for (n, line) in text.lines().enumerate() {
            let code = line.split("//").next().unwrap_or("");
            let mut rest = code;
            while let Some(i) = rest.find("\"/tmp/") {
                let lit = &rest[i + 1..];
                let end = lit[1..].find('"').map(|e| e + 1).unwrap_or(lit.len());
                let literal = &lit[..end];
                if !common::CORPUS_ROOTS.iter().any(|root| literal == *root || literal.starts_with(&format!("{root}/"))) {
                    offenders.push(format!("{name}:{}: {literal}", n + 1));
                }
                rest = &lit[end..];
            }
            if code.contains("temp_dir()") && !in_src {
                offenders.push(format!("{name}:{}: std::env::temp_dir() (use common::scratch)", n + 1));
            }
        }
    }
    assert!(offenders.is_empty(), "{} place(s) write under /tmp outside common::scratch:\n{}",
        offenders.len(), offenders.join("\n"));
}
