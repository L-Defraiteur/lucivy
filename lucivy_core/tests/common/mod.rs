//! What a test does when the corpus it reads is missing (ticket of 8 November
//! 2026, `docs/tickets/2026-11-08-un-test-de-corpus-sans-son-corpus-passe-vert.md`
//! on branch `tickets-2026-11-07`).
//!
//! A ground-truth or parity test that did not find its corpus used to pass:
//! on a synthetic corpus of eleven words, or on nothing at all (the ground-truth
//! panel returned an empty file list and ran in 0.03 s). A test that skips is
//! not green. Without its corpus a test now fails, naming the path, what it
//! found and how to recreate it. `LUCIVY_CORPUS_ABSENT_OK=1` allows the absence
//! — a synthetic corpus where the test has one, a skip said as such otherwise —
//! for the CI jobs that do not carry the corpus.
#![allow(dead_code)]

pub const ABSENT_OK: &str = "LUCIVY_CORPUS_ABSENT_OK";

pub fn absent_ok() -> bool {
    std::env::var(ABSENT_OK).is_ok_and(|v| v == "1")
}

/// How to recreate the corpus a test looked for under `root`.
pub fn recipe(root: &str) -> String {
    let kernel = "  git clone --depth=1 --branch v7.2 https://github.com/torvalds/linux ~/lucivy_bench/linux-7.2\n\
                  \x20   (the engine comparison is pinned on commit 8d3ae59288f1)\n\
                  \x20 ln -sfn ~/lucivy_bench/linux-7.2 /tmp/lucivy-cmp\n\
                  \x20 ln -sfn ~/lucivy_bench/linux-7.2 /tmp/lucivy-cmp-90k";
    let repo = format!(
        "  git clone --depth=1 https://github.com/L-Defraiteur/rag3db.git {root}\n\
         \x20 or V3_CORPUS=/path/to/another/tree (the CI uses its own checkout: V3_CORPUS=.)"
    );
    let source = if root.contains("lucivy-cmp") { kernel.to_string() } else { repo };
    format!(
        "{source}\n  the playground corpora: python3 playground/tools/build_corpus.py all\n\
         \x20 or {ABSENT_OK}=1 to run without it (a synthetic corpus or a skip, said as such)"
    )
}

/// `found` files were read under `root`, `min` are needed. `true`: go on with
/// the real corpus (its size is printed). `false`: the corpus is missing and
/// `ABSENT_OK` allows it — the caller does `substitute` (printed). Otherwise
/// the test fails with the recipe.
pub fn usable(root: &str, found: usize, min: usize, substitute: &str) -> bool {
    if found >= min {
        eprintln!("corpus: {found} files under {root}");
        return true;
    }
    if absent_ok() {
        eprintln!(
            "CORPUS ABSENT: {found} files under {root}, {min} needed — {substitute}, because {ABSENT_OK}=1"
        );
        return false;
    }
    panic!(
        "the corpus is missing: {found} files under {root}, {min} needed. Recreate it:\n{}",
        recipe(root)
    );
}

// ── Where a test writes (14 November 2026) ───────────────────────────────
//
// Tests used to write their indexes under fixed names in /tmp: never removed,
// overwritten by the next run, and 1.7 GiB of them on one machine (a RAM-backed
// /tmp). The rules, checked by `test_no_residue_under_tmp`:
// - an index or any scratch file: `scratch(label)`, a directory removed when the
//   returned guard drops, panic included;
// - a report a person reads after the run: `report_path(name)`, under the
//   target's own tmp directory (`CARGO_TARGET_TMPDIR`), not /tmp;
// - a corpus a test reads: one of `CORPUS_ROOTS`, or `rag3db_clone()`.

/// A fresh directory for one test, removed when the guard drops.
pub fn scratch(label: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(&format!("lucivy-test-{label}-"))
        .tempdir()
        .expect("create a test scratch directory")
}

/// Where an ignored diagnostic test leaves a report to read after the run.
pub fn report_path(name: &str) -> std::path::PathBuf {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("lucivy-reports");
    std::fs::create_dir_all(&dir).expect("create the report directory");
    dir.join(name)
}

/// Corpus roots tests read and never write (see `recipe`).
pub const CORPUS_ROOTS: &[&str] = &[
    "/tmp/lucivy-cmp", "/tmp/lucivy-cmp-90k", "/tmp/rag3db-bench", "/tmp/linux-bench",
    "/tmp/lucivy_parity_native",
];

/// A clone of the rag3db repository for the corpus tests (`RAG3DB_ROOT` wins):
/// kept between runs under the user's cache, not in /tmp.
pub fn rag3db_clone() -> std::path::PathBuf {
    if let Ok(root) = std::env::var("RAG3DB_ROOT") {
        return root.into();
    }
    let path = rag3db_clone_path();
    if !path.exists() {
        let parent = path.parent().unwrap();
        std::fs::create_dir_all(parent).expect("create the clone's parent");
        // Several tests of one binary call this at once: each clones into a
        // directory of its own, then renames it into place. The loser of the
        // race throws its copy away and takes the winner's.
        let staging = tempfile::Builder::new().prefix("rag3db-clone-").tempdir_in(parent)
            .expect("create the clone's staging directory");
        let target = staging.path().join("repo");
        eprintln!("Cloning https://github.com/L-Defraiteur/rag3db.git into {} ...", path.display());
        let status = std::process::Command::new("git")
            .args(["clone", "--quiet", "--depth", "1", "https://github.com/L-Defraiteur/rag3db.git"])
            .arg(&target)
            .status()
            .expect("git clone failed");
        assert!(status.success(), "git clone failed");
        if std::fs::rename(&target, &path).is_err() {
            assert!(path.exists(), "the clone could not be moved into {}", path.display());
        }
    }
    path
}

/// Where `rag3db_clone()` keeps its clone, without `RAG3DB_ROOT`.
fn rag3db_clone_path() -> std::path::PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME").map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("lucivy-tests").join("rag3db-clone")
}

/// The rag3db clone if it is already there (`RAG3DB_ROOT`, or the cache of
/// `rag3db_clone()`), without cloning: for a test that runs by default and
/// must not reach the network.
pub fn rag3db_clone_if_present() -> Option<std::path::PathBuf> {
    if let Ok(root) = std::env::var("RAG3DB_ROOT") {
        return Some(root.into());
    }
    let path = rag3db_clone_path();
    path.exists().then_some(path)
}
