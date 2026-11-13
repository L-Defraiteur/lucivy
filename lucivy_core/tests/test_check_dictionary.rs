//! `check_dictionary`: words a shard dictionary lost before 4.3.5.

use lucivy_core::directory::StdFsDirectory;
use lucivy_core::handle::LucivyHandle;
use lucivy_core::query::{self, SchemaConfig};
use lucivy_core::sharded_handle::ShardedHandle;

fn config(sfx_version: Option<u8>) -> SchemaConfig {
    SchemaConfig {
        fields: vec![
            query::FieldDef { name: "path".into(), field_type: "text".into(),
                stored: Some(true), indexed: None, fast: None },
            query::FieldDef { name: "content".into(), field_type: "text".into(),
                stored: Some(true), indexed: None, fast: None },
        ],
        sfx_version,
        ..Default::default()
    }
}

fn temp(name: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("lucivy-check-dictionary-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path
}

/// Two commits, each folded into its own dictionary generation.
fn two_folded_batches(handle: &LucivyHandle) {
    let path = handle.field("path").unwrap();
    let content = handle.field("content").unwrap();
    for (batch, word) in [(0, "aardvark"), (1, "zeppelinword")] {
        {
            let mut guard = handle.writer.lock().unwrap();
            let writer = guard.as_mut().unwrap();
            for i in 0..10 {
                let mut doc = ld_lucivy::LucivyDocument::new();
                doc.add_text(path, format!("batch{batch}/file{i}.rs"));
                doc.add_text(content, format!("{word}{i} and some shared text"));
                writer.add_document(doc).unwrap();
            }
            writer.commit().unwrap();
        }
        handle.reader.reload().unwrap();
        handle.wait_dictionary_fold();
    }
}

#[test]
fn a_healthy_index_needs_no_rebuild() {
    let dir = temp("healthy");
    let handle = LucivyHandle::create(StdFsDirectory::open(&dir).unwrap(), &config(None)).unwrap();
    two_folded_batches(&handle);
    let check = handle.check_dictionary().unwrap();
    assert!(check.concerned);
    assert!(check.segments >= 1);
    assert_eq!(check.fields.iter().map(|f| f.field.as_str()).collect::<Vec<_>>(), ["path", "content"]);
    assert!(check.fields.iter().all(|f| f.referenced > 0), "{check:?}");
    assert_eq!(check.unknown(), 0, "{check:?}");
    assert!(!check.needs_rebuild());
    assert!(check.summary().starts_with("No lost word"), "{}", check.summary());
    drop(handle);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_index_without_shard_dictionary_is_not_concerned() {
    let dir = temp("v3");
    let handle = LucivyHandle::create(StdFsDirectory::open(&dir).unwrap(), &config(Some(3))).unwrap();
    two_folded_batches(&handle);
    let check = handle.check_dictionary().unwrap();
    assert!(!check.concerned);
    assert!(!check.needs_rebuild());
    assert!(check.summary().contains("no shard dictionary"), "{}", check.summary());
    drop(handle);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The loss, reproduced on disk: the words of the second batch are in the
/// generation `meta.json` stops naming, as when their pair was deleted before
/// being folded. The segments still reference them.
#[test]
fn lost_words_are_counted_and_call_for_a_rebuild() {
    let dir = temp("lost");
    {
        let handle = LucivyHandle::create(StdFsDirectory::open(&dir).unwrap(), &config(None)).unwrap();
        two_folded_batches(&handle);
        assert_eq!(handle.check_dictionary().unwrap().unknown(), 0);
    }
    let meta_path = dir.join("meta.json");
    let mut meta: serde_json::Value = serde_json::from_slice(&std::fs::read(&meta_path).unwrap()).unwrap();
    let generations = meta["sfx_dictionary"]["generations"].as_array_mut().expect("generations");
    assert!(generations.len() >= 2, "two generations to drop one: {generations:?}");
    generations.pop();
    std::fs::write(&meta_path, serde_json::to_vec(&meta).unwrap()).unwrap();

    let handle = LucivyHandle::open(StdFsDirectory::open(&dir).unwrap()).unwrap();
    let check = handle.check_dictionary().unwrap();
    assert!(check.concerned);
    assert!(check.needs_rebuild(), "{check:?}");
    let content = check.fields.iter().find(|f| f.field == "content").unwrap();
    assert!(content.unknown > 0 && content.unknown < content.referenced, "{check:?}");
    let summary = check.summary();
    assert!(summary.contains("Rebuild this index") && summary.contains("content ("), "{summary}");
    drop(handle);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_sharded_index_is_checked_shard_by_shard() {
    let dir = temp("sharded");
    let config: SchemaConfig = serde_json::from_value(serde_json::json!({
        "fields": [{ "name": "content", "type": "text" }],
        "shards": 2
    })).unwrap();
    let index = ShardedHandle::create(dir.to_str().unwrap(), &config).unwrap();
    for i in 0..40u64 {
        index.add_document_json(i, &serde_json::json!({ "content": format!("sharded words number {i}") })).unwrap();
    }
    index.commit().unwrap();
    let check = index.check_dictionary().unwrap();
    assert!(check.concerned);
    assert_eq!(check.fields.iter().map(|f| f.field.as_str()).collect::<Vec<_>>(), ["shard_0/content", "shard_1/content"]);
    assert_eq!(check.unknown(), 0, "{check:?}");
    index.close().unwrap();
    drop(index);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The index saved on 13 November 2026 (`LUCIVY_LOST_WORDS_INDEX`, a fresh
/// copy: opening it may write): 31 862 lost ids in `content`, 480 in `path`.
#[test]
#[ignore]
fn the_saved_index_shows_its_lost_words() {
    let path = std::env::var("LUCIVY_LOST_WORDS_INDEX").expect("LUCIVY_LOST_WORDS_INDEX");
    let handle = LucivyHandle::open(StdFsDirectory::open(std::path::Path::new(&path)).unwrap()).unwrap();
    let check = handle.check_dictionary().unwrap();
    eprintln!("{check:?}\n{}", check.summary());
    let unknown = |name: &str| check.fields.iter().find(|f| f.field == name).unwrap().unknown;
    assert_eq!(unknown("content"), 31_862);
    assert_eq!(unknown("path"), 480);
    assert!(check.needs_rebuild());
}
