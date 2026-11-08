//! A missing corpus is red, unless its absence is allowed by name (ticket of
//! 8 November 2026). One test: the variable is process-wide, the two halves
//! must not run in parallel.

mod common;

#[test]
fn a_missing_corpus_is_red_unless_allowed() {
    let root = "/nonexistent/lucivy-cmp";
    // SAFETY-free in edition 2021; no other test of this binary reads the variable.
    std::env::remove_var(common::ABSENT_OK);
    let red = std::panic::catch_unwind(|| common::usable(root, 0, 50, "synthetic"));
    let message = red.expect_err("a missing corpus must fail the test");
    let text = message
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| message.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_default();
    assert!(text.contains("the corpus is missing"), "{text}");
    assert!(text.contains("git clone --depth=1 --branch v7.2"), "the recipe names the kernel: {text}");
    assert!(text.contains(common::ABSENT_OK), "the recipe names the variable: {text}");

    std::env::set_var(common::ABSENT_OK, "1");
    assert!(!common::usable(root, 0, 50, "synthetic"), "allowed: the caller substitutes");
    std::env::remove_var(common::ABSENT_OK);

    assert!(common::usable(root, 60, 50, "synthetic"), "enough files: the real corpus");
}
