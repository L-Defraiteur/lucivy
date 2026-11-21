//! A test that skips for want of its corpus (`LUCIVY_CORPUS_ABSENT_OK=1`) used to say so on its stderr only, which
//! cargo hides for a passing test: the CI log read `ok`. Each such skip is now written to the file
//! `LUCIVY_CORPUS_SKIPPED` names, and the CI step prints it in plain words after the run.

mod common;

#[test]
fn a_skip_for_want_of_a_corpus_is_written_where_the_ci_reads_it() {
    let scratch = common::scratch("corpus_skip_is_said");
    let log = scratch.path().join("skipped.txt");
    // This binary holds this test alone: setting the variables touches no other test.
    unsafe {
        std::env::set_var(common::ABSENT_OK, "1");
        std::env::set_var(common::SKIPPED_LOG, &log);
    }
    let usable = common::usable("/nowhere/a-corpus", 0, 1, "skipped, nothing is checked");
    assert!(!usable);
    let said = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        said.contains("/nowhere/a-corpus") && said.contains("a_skip_for_want_of_a_corpus_is_written_where_the_ci_reads_it"),
        "the skip is not written where the CI reads it: {said:?}"
    );
}

/// The tests of one binary run on several threads: two skips written at once must stay two whole lines.
#[test]
fn skips_written_at_once_stay_whole_lines() {
    let scratch = common::scratch("corpus_skips_at_once");
    let log = scratch.path().join("skipped.txt");
    unsafe {
        std::env::set_var(common::ABSENT_OK, "1");
        std::env::set_var(common::SKIPPED_LOG, &log);
    }
    let threads: Vec<_> = (0..16)
        .map(|t| {
            std::thread::Builder::new()
                .name(format!("skipper_{t}"))
                .spawn(move || {
                    for k in 0..50 {
                        common::usable(&format!("/nowhere/corpus-{t}-{k}"), 0, 1, "skipped, nothing is checked");
                    }
                })
                .unwrap()
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    let said = std::fs::read_to_string(&log).unwrap_or_default();
    let lines: Vec<&str> = said.lines().collect();
    assert_eq!(lines.len(), 16 * 50, "one line per skip");
    for line in lines {
        assert!(
            line.starts_with("skipper_") && line.matches("corpus absent").count() == 1 && line.ends_with("skipped, nothing is checked"),
            "a line mixed with another: {line:?}"
        );
    }
}
