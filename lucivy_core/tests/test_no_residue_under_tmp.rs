//! No test writes under a fixed path in /tmp (14 November 2026): an index goes
//! to `common::scratch`, a report to `common::report_path`, and only the corpus
//! roots of `common::CORPUS_ROOTS` are read there. Checked on the sources: a
//! test that left a directory behind is found before it runs anywhere.

mod common;

#[test]
fn no_test_writes_under_a_fixed_tmp_path() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut offenders = Vec::new();
    let mut files: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "rs")).collect();
    files.sort();
    for path in files {
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        if name == "test_no_residue_under_tmp.rs" { continue; }
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
            if code.contains("temp_dir()") {
                offenders.push(format!("{name}:{}: std::env::temp_dir() (use common::scratch)", n + 1));
            }
        }
    }
    assert!(offenders.is_empty(), "{} place(s) write under /tmp outside common::scratch:\n{}",
        offenders.len(), offenders.join("\n"));
}
