//! The walk that collects a test corpus from a directory tree, shared by the ground-truth and parity tests.
//!
//! Step 1 (18 November 2026): the walk of `test_sfx_v3_ground_truth` as it was, unchanged — `read_dir` order,
//! entries it cannot read skipped without a word, every directory not in `exclude_dirs` entered. Its witness
//! (`test_corpus_walk.rs`) is red on it.

use std::collections::BTreeMap;
use std::path::Path;

/// What a walk keeps: the files it reads, and the rules it reads them by.
pub struct Rules<'a> {
    /// Directory names never entered.
    pub exclude_dirs: &'a [&'a str],
    /// File extensions kept (`None`: every extension).
    pub extensions: Option<&'a [&'a str]>,
    /// Files larger than this are left out.
    pub max_size: u64,
    /// Leave out files of zero bytes.
    pub skip_zero_size: bool,
    /// Leave out files holding a NUL byte (binary).
    pub skip_binary: bool,
    /// Leave out files whose text is only whitespace.
    pub skip_blank: bool,
    /// Stop after this many files.
    pub max_files: usize,
    /// Name files by their path relative to the root (else the full path).
    pub relative: bool,
}

/// The files a walk kept, and what it left out, by reason.
#[derive(Default)]
pub struct Walk {
    pub files: Vec<(String, String)>,
    pub skipped: BTreeMap<&'static str, usize>,
}

pub fn walk(root: &Path, rules: &Rules) -> Walk {
    let mut out = Walk::default();
    fn visit(dir: &Path, root: &Path, rules: &Rules, out: &mut Walk) {
        if out.files.len() >= rules.max_files { return; }
        let entries = match std::fs::read_dir(dir) { Ok(e) => e, Err(_) => return };
        for entry in entries.flatten() {
            if out.files.len() >= rules.max_files { return; }
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if path.is_dir() {
                if !rules.exclude_dirs.contains(&name.as_str()) {
                    visit(&path, root, rules, out);
                }
                continue;
            }
            if !path.is_file() { continue; }
            if let Some(exts) = rules.extensions {
                let ok = path.extension().and_then(|x| x.to_str()).is_some_and(|x| exts.contains(&x));
                if !ok { continue; }
            }
            let size = path.metadata().map(|m| m.len()).unwrap_or(0);
            if size > rules.max_size || (rules.skip_zero_size && size == 0) { continue; }
            let bytes = match std::fs::read(&path) { Ok(b) => b, Err(_) => continue };
            if rules.skip_binary && bytes.contains(&0) { continue; }
            let content = match String::from_utf8(bytes) { Ok(s) => s, Err(_) => continue };
            if rules.skip_blank && content.trim().is_empty() { continue; }
            let named = if rules.relative { path.strip_prefix(root).unwrap_or(&path) } else { &path };
            out.files.push((named.to_string_lossy().to_string(), content));
        }
    }
    visit(root, root, rules, &mut out);
    out
}
