//! Lightweight witness at the SAME entry used by test_derived_in_ram.
//! No index, writer, scratch directory, real corpus or service is constructed.

mod common;

use std::cell::Cell;
use common::corpus_oracle::{self, OracleError};
use common::derived_fixture::{with_derived_fixture, FixtureError, LoadedCorpus};

fn fifty_documents_without_the_panel() -> LoadedCorpus {
    (0..50).map(|i| (format!("memory/{i}.c"), b"return 0;".to_vec())).collect()
}

#[test]
fn missing_panel_is_rejected_before_any_builder() {
    let builds = Cell::new(0);
    let result = with_derived_fixture(
        || Ok(fifty_documents_without_the_panel()),
        |_, panel| {
            assert_eq!(panel.len(), 11);
            builds.set(builds.get() + 1);
        },
    );
    // The baseline calls the builder once. This assertion must be SEEN red
    // before the missing coverage gate is corrected. Do not change the panel.
    assert_eq!(builds.get(), 0, "an uncovered panel must be refused before the builder");
    assert!(matches!(result, Err(FixtureError::UncoveredPanel { .. })));
}

#[test]
fn loader_error_is_not_an_empty_corpus_or_no_hit() {
    let builds = Cell::new(0);
    let expected = FixtureError::Read { source: "memory/broken.c".into(), reason: "read failed".into() };
    let result = with_derived_fixture(|| Err(expected), |_, _| builds.set(builds.get() + 1));
    assert!(matches!(result, Err(FixtureError::Read { .. })));
    assert_eq!(builds.get(), 0);
}

#[test]
fn invalid_utf8_is_not_silently_replaced_or_dropped() {
    let builds = Cell::new(0);
    let mut input = fifty_documents_without_the_panel();
    input[49] = ("memory/invalid.c".into(), vec![0xff]);
    let result = with_derived_fixture(|| Ok(input), |_, _| builds.set(builds.get() + 1));
    assert!(matches!(result, Err(FixtureError::Utf8 { source, .. }) if source == "memory/invalid.c"));
    assert_eq!(builds.get(), 0);
}

#[test]
fn strict_unicode_folding_maps_to_original_bytes() {
    assert_eq!(corpus_oracle::literal_spans("DÉJÀ".as_bytes(), "déjà", true).unwrap(), vec![(0, 6)]);
    assert_eq!(corpus_oracle::literal_spans("İ".as_bytes(), "i", true).unwrap(), vec![(0, 2)]);
}

#[test]
fn relaxed_spans_cross_separators_without_losing_offsets() {
    let bytes = "X DÉJÀ_vu".as_bytes();
    assert_eq!(corpus_oracle::literal_spans(bytes, "déjà vu", false).unwrap(), vec![(2, 11)]);
    assert!(corpus_oracle::literal_spans(bytes, "déjà vu", true).unwrap().is_empty());
}

#[test]
fn underscore_is_a_boundary_but_a_content_suffix_is_not() {
    let bytes = b"xsched sched_y schedz";
    let spans = corpus_oracle::literal_spans(bytes, "sched", true).unwrap();
    assert_eq!(corpus_oracle::filter_boundaries(bytes, &spans, true, true).unwrap(), vec![(7, 12)]);
}

#[test]
fn regex_keeps_nonempty_original_byte_spans() {
    let regex = regex::RegexBuilder::new("a*").case_insensitive(true).build().unwrap();
    assert_eq!(corpus_oracle::regex_spans(b"Ab", &regex).unwrap(), vec![(0, 1)]);
}

#[test]
fn fuzzy_uses_the_existing_occurrence_definition_on_admitted_bytes() {
    let bytes = b"mutex_lock";
    let spans = corpus_oracle::fuzzy_spans(bytes, "mutx_lock", 1).unwrap();
    assert!(!spans.is_empty());
    assert!(spans.iter().all(|&(from, to)| from < to && to <= bytes.len()));
}

#[test]
fn oracle_decode_failure_is_distinct_from_zero_matches() {
    assert!(matches!(corpus_oracle::literal_spans(&[0xff], "mutex_lock", true), Err(OracleError::Utf8(_))));
    assert!(corpus_oracle::literal_spans(b"return 0;", "mutex_lock", true).unwrap().is_empty());
}
