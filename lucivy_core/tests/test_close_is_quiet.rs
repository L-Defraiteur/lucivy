//! **After `close()`, nothing writes in the index directory** (22 November 2026). `main` was red twice on a temp dir
//! left by `test_fuzzy_monotonicity`: `drain_merges` polled `pending_merge_tasks`, a boolean that the first merge batch
//! to finish cleared while another batch, started by a later commit, still ran. `close()` then returned, the writer was
//! dropped (`kill`, no join), and the running batch kept writing — `StdFsDirectory` even recreates a deleted directory
//! (`create_dir_all`). Here: an undrained index, commits close enough for merge batches to overlap, `close()`, then the
//! directory must not change for a second. The race is probabilistic: ten rounds.

mod common;

use std::collections::BTreeMap;

use lucivy_core::handle::LucivyHandle;
use lucivy_core::query::{self, SchemaConfig};

fn listing(dir: &std::path::Path) -> BTreeMap<String, u64> {
    std::fs::read_dir(dir)
        .map(|it| it.flatten().map(|e| (e.file_name().to_string_lossy().to_string(), e.metadata().map(|m| m.len()).unwrap_or(0))).collect())
        .unwrap_or_default()
}

#[test]
fn nothing_writes_in_the_directory_after_close() {
    let config = SchemaConfig {
        fields: vec![query::FieldDef { name: "content".into(), field_type: "text".into(), stored: Some(true), indexed: Some(true), fast: None }],
        sfx_version: Some(3),
        ..Default::default()
    };
    for round in 0..10 {
        let scratch = common::scratch("close_is_quiet");
        let dir = lucivy_core::directory::StdFsDirectory::open(scratch.path().to_str().unwrap()).unwrap();
        let handle = LucivyHandle::create(dir, &config).unwrap();
        let content = handle.field("content").unwrap();
        {
            let mut guard = handle.writer.lock().unwrap();
            let writer = guard.as_mut().unwrap();
            for i in 0..1600u32 {
                let mut doc = ld_lucivy::LucivyDocument::new();
                doc.add_text(content, format!("pub fn function_{i}() -> u32 {{ {i} }} // rag3db_value_{i} memory safety"));
                writer.add_document(doc).unwrap();
                if (i + 1) % 100 == 0 {
                    writer.commit().unwrap();
                }
            }
        }
        handle.close().unwrap();
        let before = listing(scratch.path());
        std::thread::sleep(std::time::Duration::from_millis(1000));
        let after = listing(scratch.path());
        assert_eq!(before, after, "round {round}: the directory changed after close()");
        drop(handle);
    }
}
