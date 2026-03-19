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

    // Deep check: for first missing doc, inspect sfx directly
    eprintln!("\n=== Deep SFX inspection for shard_0 doc_298 ===");
    let shard_dir = format!("{}/shard_0", BENCH_DIR);
    let dir = lucivy_core::directory::StdFsDirectory::open(&shard_dir).unwrap();
    let index = ld_lucivy::Index::open(dir).unwrap();
    let reader = index.reader().unwrap();
    let searcher = reader.searcher();
    let schema = index.schema();
    let content_field = schema.get_field("content").unwrap();

    // Find which segment has doc 298
    let mut doc_offset = 0u32;
    for (seg_ord, seg_reader) in searcher.segment_readers().iter().enumerate() {
        let max_doc = seg_reader.max_doc();
        if 298 >= doc_offset && 298 < doc_offset + max_doc {
            let local_doc_id = 298 - doc_offset;
            eprintln!("  doc_298 is in segment {} (seg_ord={}), local_doc_id={}",
                seg_reader.segment_id().uuid_string()[..8].to_string(),
                seg_ord, local_doc_id);

            // Check term dict for "function"
            if let Ok(inv_idx) = seg_reader.inverted_index(content_field) {
                let term = ld_lucivy::Term::from_field_text(content_field, "function");
                let term_info = inv_idx.get_term_info(&term).ok().flatten();
                eprintln!("  term 'function' in term dict: {:?}", term_info.is_some());
                if let Some(ti) = term_info {
                    eprintln!("  term_info: doc_freq={}", ti.doc_freq);
                }
            }

            // Check if sfx file exists for this segment
            let has_sfx = seg_reader.sfx_file(content_field).is_some();
            eprintln!("  has_sfx for content field: {}", has_sfx);

            // Read sfxpost if available
            if let Some(sfxpost_slice) = seg_reader.sfxpost_file(content_field) {
                if let Ok(sfxpost_bytes) = sfxpost_slice.read_bytes() {
                    use ld_lucivy::suffix_fst::file::SfxPostingsReader;
                    if let Ok(sfxpost_reader) = SfxPostingsReader::open(&sfxpost_bytes) {
                        // Find ordinal for "function" in term dict
                        if let Ok(inv_idx) = seg_reader.inverted_index(content_field) {
                            let mut stream = inv_idx.terms().stream().unwrap();
                            let mut ordinal = 0u32;
                            let mut found_ord = None;
                            while stream.advance() {
                                if let Ok(s) = std::str::from_utf8(stream.key()) {
                                    if s == "function" {
                                        found_ord = Some(ordinal);
                                        break;
                                    }
                                }
                                ordinal += 1;
                            }
                            if let Some(ord) = found_ord {
                                let entries = sfxpost_reader.entries(ord);
                                let has_doc = entries.iter().any(|e| e.doc_id == local_doc_id);
                                eprintln!("  sfxpost ordinal={}: {} entries, has doc_id {}: {}",
                                    ord, entries.len(), local_doc_id, has_doc);
                                if !has_doc {
                                    // Show nearby doc_ids
                                    let nearby: Vec<u32> = entries.iter()
                                        .filter(|e| (e.doc_id as i64 - local_doc_id as i64).abs() < 5)
                                        .map(|e| e.doc_id)
                                        .collect();
                                    eprintln!("  nearby doc_ids: {:?}", nearby);
                                    eprintln!("  first 5 doc_ids: {:?}",
                                        entries.iter().take(5).map(|e| e.doc_id).collect::<Vec<_>>());
                                    eprintln!("  last 5 doc_ids: {:?}",
                                        entries.iter().rev().take(5).map(|e| e.doc_id).collect::<Vec<_>>());
                                }
                            } else {
                                eprintln!("  'function' NOT found in term dict stream!");
                            }
                        }
                    }
                }
            }
            break;
        }
        doc_offset += max_doc;
    }

    handle.close().ok();
}
