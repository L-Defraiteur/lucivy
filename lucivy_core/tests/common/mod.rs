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
