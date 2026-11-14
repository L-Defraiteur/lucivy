//! A merge that starts between two commits must not lose the words minted by
//! the segments written since the last commit (shard dictionary, sfx v4).
//!
//! The merge's preparation used to commit every segment in the segment
//! manager, the uncommitted ones included, without folding their texts. The
//! next commit folds only the segments not yet committed, so it skipped them,
//! and their minted pair was deleted as consumed: the words existed in no
//! dictionary generation. A fuzzy `contains` then dropped every document using
//! them (the text window cannot be rebuilt), while d=0 kept them.

mod common;

use lucivy_core::directory::StdFsDirectory;
use lucivy_core::handle::LucivyHandle;
use lucivy_core::query::{self, QueryConfig, SchemaConfig};
use std::collections::BTreeSet;

fn contents(handle: &LucivyHandle, value: &str, distance: u8) -> BTreeSet<String> {
    let qconfig = QueryConfig {
        query_type: "contains".into(),
        field: Some("content".into()),
        value: Some(value.to_string()),
        distance: Some(distance),
        ..Default::default()
    };
    let results = handle.search(&qconfig, 10_000, None).unwrap();
    // Resolved with the reader as it stands right after the search.
    let searcher = handle.reader.searcher();
    let content = handle.field("content").unwrap();
    results.iter().map(|(_, addr)| {
        let doc: ld_lucivy::LucivyDocument = searcher.doc(*addr).unwrap();
        doc.get_first(content).map(|v| {
            let o: ld_lucivy::schema::OwnedValue = v.into();
            match o { ld_lucivy::schema::OwnedValue::Str(s) => s, _ => String::new() }
        }).unwrap_or_default()
    }).collect()
}

#[test]
fn a_merge_between_commits_keeps_the_new_segments_words() { run(true) }

/// The same path without the merge: the witness of the witness.
#[test]
fn without_a_merge_the_new_segments_words_are_found() { run(false) }

fn run(merge: bool) {
    let scratch = common::scratch(&format!("lucivy-merge-new-words-{merge}"));
    let path = scratch.path().to_path_buf();
    let config = SchemaConfig {
        fields: vec![query::FieldDef { name: "content".into(), field_type: "text".into(),
            stored: Some(true), indexed: Some(true), fast: None }],
        ..Default::default()
    };
    let new_docs: BTreeSet<String> = (0..20).map(|i| format!("quokkaword{i:02} lives here")).collect();
    {
        let handle = LucivyHandle::create(StdFsDirectory::open(&path).unwrap(), &config).unwrap();
        let content = handle.field("content").unwrap();
        let mut guard = handle.writer.lock().unwrap();
        let writer = guard.as_mut().unwrap();
        writer.set_merge_policy(Box::new(ld_lucivy::indexer::NoMergePolicy));
        // Two committed batches: at least two committed segments to merge.
        for batch in 0..2 {
            for i in 0..20 {
                let mut doc = ld_lucivy::LucivyDocument::new();
                doc.add_text(content, format!("ordinary text batch {batch} number {i}"));
                writer.add_document(doc).unwrap();
            }
            writer.commit().unwrap();
        }
        // New words, flushed into a segment but not committed yet.
        for text in &new_docs {
            let mut doc = ld_lucivy::LucivyDocument::new();
            doc.add_text(content, text);
            writer.add_document(doc).unwrap();
        }
        drop(writer.prepare_commit().unwrap());
        // A merge of the committed segments starts between the two commits.
        if merge {
            let committed = handle.index.searchable_segment_ids().unwrap();
            assert!(committed.len() >= 2, "two committed segments to merge, got {}", committed.len());
            writer.merge(&committed).unwrap();
        }
        writer.commit().unwrap();
        drop(guard);
        handle.reader.reload().unwrap();
        handle.wait_dictionary_fold();
        assert_eq!(contents(&handle, "quokkaword", 0), new_docs, "d=0 before reopening");
        assert_eq!(contents(&handle, "quokkaword", 1), new_docs, "d=1 before reopening");
    }
    // Reopened: the dictionary is what the disk holds.
    let handle = LucivyHandle::open(StdFsDirectory::open(&path).unwrap()).unwrap();
    assert_eq!(contents(&handle, "quokkaword", 0), new_docs, "d=0 after reopening");
    assert_eq!(contents(&handle, "quokkaword", 1), new_docs,
        "d=1 after reopening: a document whose words are in no dictionary generation is dropped by fuzzy");
    drop(handle);
}
