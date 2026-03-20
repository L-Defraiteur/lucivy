//! Investigation: why does "lock" miss docs on 90K bench?

#[test]
fn investigate_lock_search_path() {
    let shard_dir = "/home/luciedefraiteur/lucivy_bench_sharding/token_aware/shard_0";
    if !std::path::Path::new(shard_dir).exists() {
        eprintln!("Skipping: no persisted index");
        return;
    }

    let dir = lucivy_core::directory::StdFsDirectory::open(shard_dir).unwrap();
    let handle = lucivy_core::handle::LucivyHandle::open(dir).unwrap();
    let field = handle.field("content").unwrap();
    let searcher = handle.reader.searcher();

    // Run the real search via the public Query API
    let rx = ld_lucivy::diag::diag_bus().subscribe(
        ld_lucivy::diag::DiagFilter::SfxTerm("lock".to_string()),
    );

    let q = ld_lucivy::query::SuffixContainsQuery::new(field, "lock".to_string())
        .with_continuation(true);
    let count = searcher.search(&q, &ld_lucivy::collector::Count).unwrap();

    // Collect search doc_ids from events
    let mut search_docs = std::collections::HashSet::new();
    while let Ok(event) = rx.try_recv() {
        if let ld_lucivy::diag::DiagEvent::SearchMatch { doc_id, .. } = event {
            search_docs.insert(doc_id);
        }
    }
    ld_lucivy::diag::diag_bus().clear();

    eprintln!("Search count (collector): {count}");
    eprintln!("Search docs (events): {}", search_docs.len());

    // Ground truth + find missing
    let mut gt_count = 0u32;
    let mut missing = Vec::new();
    for seg_reader in searcher.segment_readers() {
        if let Ok(store) = seg_reader.get_store_reader(0) {
            for doc_id in 0..seg_reader.max_doc() {
                if seg_reader.alive_bitset().map_or(true, |bs| bs.is_alive(doc_id)) {
                    if let Ok(doc) = store.get::<ld_lucivy::LucivyDocument>(doc_id) {
                        for (_f, val) in doc.field_values() {
                            use ld_lucivy::schema::document::Value;
                            if let Some(text) = val.as_value().as_str() {
                                if text.to_lowercase().contains("lock") {
                                    gt_count += 1;
                                    if !search_docs.contains(&doc_id) && missing.len() < 10 {
                                        // Find which token has "lock"
                                        let tm = handle.index.tokenizers();
                                        let mut tokens_with_lock = Vec::new();
                                        if let Some(mut analyzer) = tm.get("raw_code") {
                                            let mut s = analyzer.token_stream(text);
                                            while s.advance() {
                                                let tok = s.token();
                                                if tok.text.contains("lock") {
                                                    tokens_with_lock.push(tok.text.clone());
                                                }
                                            }
                                        }
                                        missing.push((doc_id, tokens_with_lock));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    eprintln!("Ground truth: {gt_count}");
    eprintln!("Diff: {}", gt_count as i64 - search_docs.len() as i64);

    if !missing.is_empty() {
        eprintln!("\nMissing docs:");
        for (doc_id, tokens) in &missing {
            eprintln!("  doc {doc_id}: tokens with 'lock': {tokens:?}");
        }
    }
}
