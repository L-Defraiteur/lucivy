use std::path::Path;

use ld_lucivy::directory::{Directory, ManagedDirectory, RamDirectory, TerminatingWrite};
use ld_lucivy::schema::{Schema, TEXT};
use ld_lucivy::{doc, Index, IndexWriter, Term};

#[test]
fn test_failpoints_managed_directory_gc_if_delete_fails() {
    let _scenario = fail::FailScenario::setup();

    let test_path: &'static Path = Path::new("some_path_for_test");

    let ram_directory = Box::new(RamDirectory::create());
    let mut managed_directory = ManagedDirectory::wrap(ram_directory).unwrap();
    managed_directory
        .open_write(test_path)
        .unwrap()
        .terminate()
        .unwrap();
    assert!(managed_directory.exists(test_path).unwrap());
    // triggering gc and setting the delete operation to fail.
    //
    // We are checking that the gc operation is not removing the
    // file from managed.json to ensure that the file will be removed
    // in the next gc.
    //
    // The initial 1*off is there to allow for the removal of the
    // lock file.
    fail::cfg("RamDirectory::delete", "1*off->1*return").unwrap();
    assert!(managed_directory.garbage_collect(Default::default).is_ok());
    assert!(managed_directory.exists(test_path).unwrap());

    // running the gc a second time should remove the file.
    assert!(managed_directory.garbage_collect(Default::default).is_ok());
    assert!(
        !managed_directory.exists(test_path).unwrap(),
        "The file should have been deleted"
    );
}

#[test]
fn test_write_commit_fails() -> ld_lucivy::Result<()> {
    let _fail_scenario_guard = fail::FailScenario::setup();
    let mut schema_builder = Schema::builder();
    let text_field = schema_builder.add_text_field("text", TEXT);
    let index = Index::create_in_ram(schema_builder.build());

    let mut index_writer: IndexWriter = index.writer_with_num_threads(1, 15_000_000)?;
    for _ in 0..100 {
        index_writer.add_document(doc!(text_field => "a"))?;
    }
    index_writer.commit()?;
    fail::cfg("save_metas", "return(error_write_failed)").unwrap();
    for _ in 0..100 {
        index_writer.add_document(doc!(text_field => "b"))?;
    }
    assert!(index_writer.commit().is_err());

    let num_docs_containing = |s: &str| {
        let term_a = Term::from_field_text(text_field, s);
        index.reader()?.searcher().doc_freq(&term_a)
    };
    assert_eq!(num_docs_containing("a")?, 100);
    assert_eq!(num_docs_containing("b")?, 0);
    Ok(())
}

// Motivated by
// - https://github.com/quickwit-oss/quickwit/issues/730
// Details at
// - https://github.com/quickwit-oss/tantivy/issues/1198
#[test]
fn test_fail_on_flush_segment() -> ld_lucivy::Result<()> {
    let _fail_scenario_guard = fail::FailScenario::setup();
    let mut schema_builder = Schema::builder();
    let text_field = schema_builder.add_text_field("text", TEXT);
    let index = Index::create_in_ram(schema_builder.build());
    let index_writer: IndexWriter = index.writer_with_num_threads(1, 15_000_000)?;
    fail::cfg("FieldSerializer::close_term", "return(simulatederror)").unwrap();
    for i in 0..100_000 {
        if index_writer
            .add_document(doc!(text_field => format!("hellohappytaxpayerlongtokenblabla{}", i)))
            .is_err()
        {
            return Ok(());
        }
    }
    panic!("add_document should have returned an error");
}

#[test]
fn test_fail_on_flush_segment_but_one_worker_remains() -> ld_lucivy::Result<()> {
    let _fail_scenario_guard = fail::FailScenario::setup();
    let mut schema_builder = Schema::builder();
    let text_field = schema_builder.add_text_field("text", TEXT);
    let index = Index::create_in_ram(schema_builder.build());
    let index_writer: IndexWriter = index.writer_with_num_threads(2, 30_000_000)?;
    fail::cfg("FieldSerializer::close_term", "1*return(simulatederror)").unwrap();
    for i in 0..100_000 {
        if index_writer
            .add_document(doc!(text_field => format!("hellohappytaxpayerlongtokenblabla{}", i)))
            .is_err()
        {
            return Ok(());
        }
    }
    panic!("add_document should have returned an error");
}

#[test]
fn test_fail_on_commit_segment() -> ld_lucivy::Result<()> {
    let _fail_scenario_guard = fail::FailScenario::setup();
    let mut schema_builder = Schema::builder();
    let text_field = schema_builder.add_text_field("text", TEXT);
    let index = Index::create_in_ram(schema_builder.build());
    let mut index_writer: IndexWriter = index.writer_with_num_threads(1, 15_000_000)?;
    fail::cfg("FieldSerializer::close_term", "return(simulatederror)").unwrap();
    for i in 0..10 {
        index_writer
            .add_document(doc!(text_field => format!("hellohappytaxpayerlongtokenblabla{}", i)))?;
    }
    assert!(index_writer.commit().is_err());
    Ok(())
}

/// A RAM index with a shard dictionary (`sfx_version` 4): `create_in_ram` alone has none.
fn dictionary_index(schema: Schema) -> Index {
    let settings = ld_lucivy::IndexSettings { sfx_version: 4, ..Default::default() };
    let index = ld_lucivy::IndexBuilder::new().schema(schema).settings(settings).create_in_ram().unwrap();
    assert!(index.sfx_dictionary().is_some(), "a dictionary index");
    index
}

/// Ids that the live segments' `.gmap`s cite and no live dictionary generation holds a text for: words lost.
fn lost_words(index: &Index) -> usize {
    use ld_lucivy::suffix_fst::gmap::GmapReader;
    let reader = index.reader().unwrap();
    reader.reload().unwrap();
    let dictionary = index.sfx_dictionary().expect("a dictionary index");
    let mut lost = 0;
    for segment in reader.searcher().segment_readers() {
        for field in segment.sfx_fields().collect::<Vec<_>>() {
            let Some(file) = segment.sfx_index_file("gmap", field) else { continue };
            let bytes = file.read_bytes().unwrap();
            let texts = dictionary.field(field.field_id()).and_then(|f| f.termtexts_reader());
            for id in GmapReader::open(bytes.as_slice()).unwrap().iter() {
                if !texts.as_ref().is_some_and(|t| t.text(id).is_some()) {
                    lost += 1;
                }
            }
        }
    }
    lost
}

/// **A compaction that did not see a segment keeps its texts one more cycle** (19 November 2026, the lost words
/// of rag3weaver's code base). The compaction of the shard dictionary keeps the ids the committed segments'
/// `.gmap`s cite; on the host, a segment outside that view — the failpoint hides one here — lost the texts only it
/// cited, silently. The dead texts now go to a reprieve generation, still read, for one cycle; the next
/// compaction keeps those a `.gmap` cites again and names the segment that cites them.
#[test]
fn test_failpoints_a_compaction_that_misses_a_segment_loses_no_word() -> ld_lucivy::Result<()> {
    let _scenario = fail::FailScenario::setup();
    let mut schema_builder = Schema::builder();
    let text_field = schema_builder.add_text_field("text", TEXT);
    let index = dictionary_index(schema_builder.build());
    let mut index_writer: IndexWriter = index.writer_with_num_threads(1, 15_000_000)?;
    index_writer.add_document(doc!(text_field => "fn seenword alpha"))?;
    index_writer.commit()?;
    index_writer.add_document(doc!(text_field => "fn hiddenword beta"))?;
    index_writer.commit()?;
    assert_eq!(lost_words(&index), 0);

    fail::cfg("compact_dictionary::hide_segments", "1*return").unwrap();
    index_writer.compact_dictionary()?;
    fail::remove("compact_dictionary::hide_segments");
    assert_eq!(lost_words(&index), 0, "the texts of the hidden segment are in reprieve, still read");

    let report = index_writer.compact_dictionary()?;
    assert!(!report.revived.is_empty(), "the next compaction keeps the texts cited again: {report:?}");
    assert!(report.revived.iter().all(|r| r.contains("segment ")), "and names the segment: {:?}", report.revived);
    assert_eq!(lost_words(&index), 0, "kept after the reprieve");
    index_writer.compact_dictionary()?;
    assert_eq!(lost_words(&index), 0, "kept for good");
    Ok(())
}

/// **A dead text leaves after its reprieve** (19 November 2026): a word only a deleted document held goes to the
/// reprieve generation at the first compaction, and out of the dictionary at the second.
#[test]
fn test_failpoints_a_dead_text_leaves_after_its_reprieve() -> ld_lucivy::Result<()> {
    let _scenario = fail::FailScenario::setup();
    let mut schema_builder = Schema::builder();
    let text_field = schema_builder.add_text_field("text", TEXT);
    let index = dictionary_index(schema_builder.build());
    let mut index_writer: IndexWriter = index.writer_with_num_threads(1, 15_000_000)?;
    index_writer.add_document(doc!(text_field => "fn keptword alpha"))?;
    index_writer.add_document(doc!(text_field => "fn deadword gamma"))?;
    index_writer.commit()?;
    index_writer.delete_term(Term::from_field_text(text_field, "deadword"));
    index_writer.commit()?;
    // A merge drops the dead document's ids from the `.gmap`.
    index_writer.compact(1_000_000)?;
    let first = index_writer.compact_dictionary()?;
    let second = index_writer.compact_dictionary()?;
    assert!(first.reprieved > 0, "the first compaction puts the dead texts in reprieve: {first:?}");
    assert!(second.texts_after < first.texts_after + first.reprieved, "the second one lets them go: {first:?} then {second:?}");
    assert!(second.revived.is_empty(), "nothing came back: {second:?}");
    assert_eq!(lost_words(&index), 0);
    Ok(())
}
