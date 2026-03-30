//! Test: import the playground .luce snapshot and search it natively.
//! Same dataset as the browser playground — validates core search on real data.

use lucivy_core::handle::LucivyHandle;
use lucivy_core::query::{self, QueryConfig};
use lucivy_core::snapshot;
use std::sync::Arc;

fn search_with_highlights(handle: &LucivyHandle, config: &QueryConfig) -> Vec<(f32, u32, Vec<[usize; 2]>)> {
    let sink = Arc::new(ld_lucivy::query::HighlightSink::new());
    let q = query::build_query(config, &handle.schema, &handle.index, Some(sink.clone())).unwrap();
    let searcher = handle.reader.searcher();
    let collector = ld_lucivy::collector::TopDocs::with_limit(20).order_by_score();
    let results = searcher.search(&*q, &collector).unwrap();
    results.iter().map(|(score, addr)| {
        let seg_id = searcher.segment_reader(addr.segment_ord as u32).segment_id();
        let hl_map = sink.get(seg_id, addr.doc_id);
        let content_hl = hl_map
            .and_then(|m| m.get("content").cloned())
            .unwrap_or_default();
        (*score, addr.doc_id, content_hl)
    }).collect()
}

#[test]
fn test_luce_playground_search() {
    let luce_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../playground/dataset.luce");

    if !luce_path.exists() {
        eprintln!("Skipping: no playground/dataset.luce");
        return;
    }

    let data = std::fs::read(&luce_path).unwrap();
    eprintln!(".luce loaded: {} bytes", data.len());

    let dest = std::path::Path::new("/tmp/test_luce_native_import");
    let _ = std::fs::remove_dir_all(dest);
    std::fs::create_dir_all(dest).unwrap();

    let handle = snapshot::import_index(&data, dest).unwrap();
    let ndocs = handle.reader.searcher().num_docs();
    eprintln!("Imported: {} docs\n", ndocs);

    // === Contains exact ===
    for q in ["rag3weaver", "weaver", "rag3db"] {
        let results = search_with_highlights(&handle, &QueryConfig {
            query_type: "contains".into(),
            field: Some("content".into()),
            value: Some(q.to_string()),
            ..Default::default()
        });
        eprintln!("contains \"{}\": {} results", q, results.len());
        assert!(results.len() > 0, "contains \"{}\" should find results", q);
    }

    // === Fuzzy d=1 with ALL highlights like the playground does ===
    eprintln!();
    for (query_text, dist) in [("rak3weaver", 1), ("rag3weavr", 1), ("weavr", 1), ("rak3db", 1)] {
        let sink = Arc::new(ld_lucivy::query::HighlightSink::new());
        let config = QueryConfig {
            query_type: "contains".into(),
            field: Some("content".into()),
            value: Some(query_text.to_string()),
            distance: Some(dist),
            ..Default::default()
        };
        let q = query::build_query(&config, &handle.schema, &handle.index, Some(sink.clone())).unwrap();
        let searcher = handle.reader.searcher();
        let collector = ld_lucivy::collector::TopDocs::with_limit(5).order_by_score();
        let results = searcher.search(&*q, &collector).unwrap();
        eprintln!("fuzzy \"{}\" d={}: {} results", query_text, dist, results.len());

        for (score, addr) in &results {
            let seg_id = searcher.segment_reader(addr.segment_ord as u32).segment_id();
            let hl_map = sink.get(seg_id, addr.doc_id);

            // Get the stored content to show highlighted text
            let doc: ld_lucivy::LucivyDocument = searcher.doc(*addr).unwrap();
            let content_field = handle.field("content").unwrap();
            let content: String = doc.get_first(content_field)
                .map(|v| {
                    let owned: ld_lucivy::schema::OwnedValue = v.into();
                    match owned {
                        ld_lucivy::schema::OwnedValue::Str(s) => s,
                        _ => String::new(),
                    }
                })
                .unwrap_or_default();

            if let Some(fields) = hl_map {
                if let Some(offsets) = fields.get("content") {
                    for hl in offsets {
                        let start = hl[0];
                        let end = hl[1].min(content.len());
                        let matched = &content[start..end];
                        let mut context_start = start.saturating_sub(10);
                        while context_start > 0 && !content.is_char_boundary(context_start) {
                            context_start -= 1;
                        }
                        let mut context_end = (end + 10).min(content.len());
                        while context_end < content.len() && !content.is_char_boundary(context_end) {
                            context_end += 1;
                        }
                        let context = &content[context_start..context_end];
                        eprintln!("  doc={} score={:.4} hl=[{},{}] = \"{}\"  context: ...{}...",
                            addr.doc_id, score, start, end, matched, context);
                    }
                } else {
                    eprintln!("  doc={} score={:.4} NO content highlights", addr.doc_id, score);
                }
            } else {
                eprintln!("  doc={} score={:.4} NO highlights at all", addr.doc_id, score);
            }
        }
        eprintln!();
    }

    // === Regex ===
    eprintln!();
    let results = search_with_highlights(&handle, &QueryConfig {
        query_type: "contains".into(),
        field: Some("content".into()),
        value: Some("rag3.*ver".to_string()),
        regex: Some(true),
        ..Default::default()
    });
    eprintln!("regex \"rag3.*ver\": {} results", results.len());
    assert!(results.len() > 0, "regex should find results");

    // === Multi-token contains (d=0) ===
    eprintln!();
    let multi_queries = [
        "use rag3weaver",
        "use rag3weaver for",
        "use rag3weaver for search",
        "rag3weaver for search",
    ];
    for q in &multi_queries {
        let results = search_with_highlights(&handle, &QueryConfig {
            query_type: "contains".into(),
            field: Some("content".into()),
            value: Some(q.to_string()),
            ..Default::default()
        });
        eprintln!("contains d=0 \"{}\": {} results", q, results.len());
    }

    // === Multi-token fuzzy (d=1) ===
    for q in ["use rak3weaver for search", "rak3weaver for search"] {
        let results = search_with_highlights(&handle, &QueryConfig {
            query_type: "contains".into(),
            field: Some("content".into()),
            value: Some(q.to_string()),
            distance: Some(1),
            ..Default::default()
        });
        eprintln!("contains d=1 \"{}\": {} results", q, results.len());
    }

    eprintln!("\nAll .luce native tests passed!");
}
