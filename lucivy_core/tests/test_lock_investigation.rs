//! Investigation: why does "lock" miss docs with tokens like "clock", "block"?

#[test]
fn repro_lock_small() {
    // Create a tiny index with docs containing "clock", "block", "lock"
    use lucivy_core::handle::LucivyHandle;
    use ld_lucivy::directory::RamDirectory;

    let dir = RamDirectory::create();
    let config: lucivy_core::query::SchemaConfig = serde_json::from_value(serde_json::json!({
        "fields": [{"name": "content", "type": "text", "stored": true}]
    })).unwrap();

    let mut handle = LucivyHandle::create(dir.clone(), &config).unwrap();
    let field = handle.field("content").unwrap();

    // Add docs with various tokens containing "lock"
    let docs = vec![
        "the clock is ticking",           // "clock" contains "lock" at SI=1
        "check the block device",          // "block" contains "lock" at SI=1
        "acquire the lock please",         // "lock" exact match SI=0
        "unlock the mutex",                // "unlock" contains "lock" at SI=2
        "this is a blockdev thing",        // "blockdev" contains "lock" at SI=1
        "no match here at all",            // no "lock"
        "the clockwork orange",            // "clockwork" contains "lock" at SI=1
        "deadlock prevention",             // "deadlock" contains "lock" at SI=4
        "memblock allocation",             // "memblock" contains "lock" at SI=4
    ];

    {
        let mut guard = handle.writer.lock().unwrap();
        let w = guard.as_mut().unwrap();
        for text in &docs {
            let mut doc = ld_lucivy::LucivyDocument::new();
            doc.add_text(field, text);
            w.add_document(doc).unwrap();
        }
        w.commit().unwrap();
    }
    // Reload reader to see committed docs
    handle.reader.reload().unwrap();

    let searcher = handle.reader.searcher();

    // Subscribe to events
    let rx = ld_lucivy::diag::diag_bus().subscribe(
        ld_lucivy::diag::DiagFilter::SfxTerm("lock".to_string()),
    );

    // Run search
    let q = ld_lucivy::query::SuffixContainsQuery::new(field, "lock".to_string());
    let count = searcher.search(&q, &ld_lucivy::collector::Count).unwrap();

    // Collect events
    let mut search_docs = std::collections::HashSet::new();
    while let Ok(event) = rx.try_recv() {
        if let ld_lucivy::diag::DiagEvent::SearchMatch { doc_id, .. } = event {
            search_docs.insert(doc_id);
        }
    }
    ld_lucivy::diag::diag_bus().clear();

    eprintln!("Search count: {count}");
    eprintln!("Search docs from events: {:?}", search_docs);

    // Ground truth
    let mut gt_docs = std::collections::HashSet::new();
    for seg_reader in searcher.segment_readers() {
        if let Ok(store) = seg_reader.get_store_reader(0) {
            for doc_id in 0..seg_reader.max_doc() {
                if let Ok(doc) = store.get::<ld_lucivy::LucivyDocument>(doc_id) {
                    for (_f, val) in doc.field_values() {
                        use ld_lucivy::schema::document::Value;
                        if let Some(text) = val.as_value().as_str() {
                            if text.to_lowercase().contains("lock") {
                                gt_docs.insert(doc_id);
                            }
                        }
                    }
                }
            }
        }
    }

    eprintln!("Ground truth docs: {:?}", gt_docs);
    eprintln!("Ground truth count: {}", gt_docs.len());

    let missing: Vec<u32> = gt_docs.difference(&search_docs).copied().collect();
    eprintln!("Missing: {:?}", missing);

    // Trace each missing doc
    for &doc_id in &missing {
        let trace = lucivy_core::diagnostics::trace_search(&handle, "content", "lock", doc_id);
        eprintln!("\n{}", trace);
    }

    assert_eq!(missing.len(), 0, "Search should find all docs containing 'lock'");
}
