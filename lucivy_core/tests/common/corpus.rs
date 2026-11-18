//! The walk that collects a test corpus from a directory tree, shared by the ground-truth and parity tests.
//!
//! Until 18 November 2026 each test had its own copy of this walk: in `read_dir` order (under a cap, the files
//! kept changed from one file system to another), leaving out what it could not read without a word, and
//! entering `.vault/` like any other directory. The walk now sorts each directory, counts what it leaves out by
//! reason ([`Walk::report`]), and never reads a secret path (`.vault`, `.env*`). Witness: `test_corpus_walk.rs`.

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

impl Walk {
    fn skip(&mut self, reason: &'static str) {
        *self.skipped.entry(reason).or_insert(0) += 1;
    }

    /// One line: the files kept and those left out, by reason.
    pub fn report(&self, root: &Path) -> String {
        let left: Vec<String> = self.skipped.iter().map(|(r, n)| format!("{n} {r}")).collect();
        format!(
            "corpus walk of {}: {} files kept; left out: {}",
            root.display(),
            self.files.len(),
            if left.is_empty() { "none".to_string() } else { left.join(", ") }
        )
    }
}

/// A path a test must never read: a `.vault` directory, an `.env` file or any `.env.*`.
pub fn is_secret(name: &str) -> bool {
    name == ".vault" || name == ".env" || name.starts_with(".env.")
}

/// Walk `root` by `rules`: each directory in name order, so a cap keeps the same files on every file system.
pub fn walk(root: &Path, rules: &Rules) -> Walk {
    let mut out = Walk::default();
    fn visit(dir: &Path, root: &Path, rules: &Rules, out: &mut Walk) {
        if out.files.len() >= rules.max_files { return; }
        let mut entries: Vec<std::fs::DirEntry> = match std::fs::read_dir(dir) {
            Ok(rd) => rd.filter_map(|e| e.map_err(|_| out.skip("unreadable entry")).ok()).collect(),
            Err(_) => return out.skip("unreadable directory"),
        };
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            if out.files.len() >= rules.max_files { return; }
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if is_secret(&name) {
                out.skip("secret path");
                continue;
            }
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
            let size = match path.metadata() {
                Ok(m) => m.len(),
                Err(_) => { out.skip("unreadable metadata"); continue; }
            };
            if size > rules.max_size { out.skip("too large"); continue; }
            if rules.skip_zero_size && size == 0 { out.skip("empty"); continue; }
            let bytes = match std::fs::read(&path) {
                Ok(b) => b,
                Err(_) => { out.skip("unreadable file"); continue; }
            };
            if rules.skip_binary && bytes.contains(&0) { out.skip("binary"); continue; }
            let content = match String::from_utf8(bytes) {
                Ok(s) => s,
                Err(_) => { out.skip("not UTF-8"); continue; }
            };
            if rules.skip_blank && content.trim().is_empty() { out.skip("blank"); continue; }
            let named = if rules.relative { path.strip_prefix(root).unwrap_or(&path) } else { &path };
            out.files.push((named.to_string_lossy().to_string(), content));
        }
    }
    visit(root, root, rules, &mut out);
    out
}
