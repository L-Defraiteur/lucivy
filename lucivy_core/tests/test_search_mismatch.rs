//! Diagnostic: find specific docs missing from contains search.

use ld_lucivy::schema::Value;
use lucivy_core::sharded_handle::ShardedHandle;
use lucivy_core::query::QueryConfig;
use std::collections::HashSet;

const BENCH_DIR: &str = "/home/luciedefraiteur/lucivy_bench_sharding/token_aware";

#[test]
fn diagnose_missing_function_docs() {
    let handle = match ShardedHandle::open(BENCH_DIR) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("Cannot open index: {} — run bench first", e);
            return;
        }
    };

    let term = "function";
    eprintln!("=== Finding docs with {:?} in text but NOT in search ===\n", term);

    // 1. Get all search hits
    let config = QueryConfig {
        query_type: "contains".to_string(),
        field: Some("content".to_string()),
        value: Some(term.to_string()),
        ..Default::default()
    };
    let results = handle.search(&config, 100000, None).unwrap();

    // Collect (shard_id, node_id) from search results
    // We need node_id to compare — but search returns (shard_id, DocAddress)
    // node_id is stored in the doc. Let's collect by shard+doc_id for now.
    let mut search_hits: HashSet<(usize, u32)> = HashSet::new();
    for r in &results {
        search_hits.insert((r.shard_id, r.doc_address.doc_id));
    }
    eprintln!("Search found {} docs\n", search_hits.len());

    // 2. Iterate ALL stored docs in ALL shards, find ones containing term
    let reports = lucivy_core::diagnostics::inspect_term_sharded_verified(
        &handle, "content", term,
    );

    // The ground truth is computed inside inspect_term_sharded_verified.
    // But we need the actual doc texts. Let's access shards directly.
    // ShardedHandle doesn't expose shards — use the diagnostic API differently.

    // Actually, let's use the single-shard diagnostics per shard.
    // We need access to the LucivyHandles. Let's check if there's an API.

    // For now: re-run the ground truth check ourselves and print the missing docs.
    eprintln!("(Iterating stored docs to find mismatches...)\n");

    // We can open each shard directory directly
    for shard_id in 0..handle.num_shards() {
        let shard_dir = format!("{}/shard_{}", BENCH_DIR, shard_id);
        let dir = lucivy_core::directory::StdFsDirectory::open(&shard_dir).unwrap();
        let index = match ld_lucivy::Index::open(dir) {
            Ok(i) => i,
            Err(e) => {
                eprintln!("  Cannot open shard_{}: {}", shard_id, e);
                continue;
            }
        };
        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        let schema = index.schema();
        let content_field = schema.get_field("content").unwrap();

        for seg_reader in searcher.segment_readers() {
            let store = match seg_reader.get_store_reader(0) {
                Ok(s) => s,
                Err(_) => continue,
            };
            for doc_id in 0..seg_reader.max_doc() {
                if !seg_reader.alive_bitset().map_or(true, |bs| bs.is_alive(doc_id)) {
                    continue;
                }
                if let Ok(doc) = store.get::<ld_lucivy::LucivyDocument>(doc_id) {
                    let mut text_content = String::new();
                    for (f, val) in doc.field_values() {
                        if f == content_field {
                            if let Some(text) = val.as_value().as_str() {
                                text_content = text.to_string();
                                break;
                            }
                        }
                    }

                    if text_content.to_lowercase().contains(term) {
                        // This doc has "function" in text. Is it in search results?
                        if !search_hits.contains(&(shard_id, doc_id)) {
                            // MISSING from search!
                            let pos = text_content.to_lowercase().find(term).unwrap();
                            let start = pos.saturating_sub(30);
                            let end = (pos + term.len() + 30).min(text_content.len());
                            let snippet = text_content[start..end].replace('\n', " ");

                            // Get the path field for identification
                            let path_field = schema.get_field("path").unwrap();
                            let path = doc.field_values()
                                .find(|(f, _)| *f == path_field)
                                .and_then(|(_, v)| v.as_value().as_str().map(|s| s.to_string()))
                                .unwrap_or_default();

                            eprintln!("MISSING shard={} doc={}: {}", shard_id, doc_id, path);
                            eprintln!("  ...{}...", snippet);

                            // Check: what tokens does the tokenizer produce for this text?
                            // Find the specific word containing "function"
                            let lower = text_content.to_lowercase();
                            let mut word_start = pos;
                            while word_start > 0 && lower.as_bytes()[word_start - 1].is_ascii_alphanumeric() {
                                word_start -= 1;
                            }
                            let mut word_end = pos + term.len();
                            while word_end < lower.len() && lower.as_bytes()[word_end].is_ascii_alphanumeric() {
                                word_end += 1;
                            }
                            let containing_word = &lower[word_start..word_end];
                            eprintln!("  containing word: {:?}", containing_word);
                            eprintln!();
                        }
                    }
                }
            }
        }
    }

    handle.close().ok();
}
