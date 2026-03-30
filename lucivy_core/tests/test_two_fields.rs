// Quick test: 2 text fields → parallel DAG path → check posmap
use lucivy_core::handle::LucivyHandle;
use lucivy_core::query::{self, QueryConfig, SchemaConfig};
use lucivy_core::directory::StdFsDirectory;

#[test]
fn test_two_text_fields_posmap() {
    let tmp = std::path::Path::new("/tmp/test_two_fields_posmap");
    let _ = std::fs::remove_dir_all(tmp);
    std::fs::create_dir_all(tmp).unwrap();
    
    let config = SchemaConfig {
        fields: vec![
            query::FieldDef { name: "path".into(), field_type: "text".into(), stored: Some(true), indexed: Some(true), fast: None },
            query::FieldDef { name: "content".into(), field_type: "text".into(), stored: Some(true), indexed: Some(true), fast: None },
        ],
        ..Default::default()
    };
    let dir = StdFsDirectory::open(tmp).unwrap();
    let handle = LucivyHandle::create(dir, &config).unwrap();
    
    {
        let mut guard = handle.writer.lock().unwrap();
        let writer = guard.as_mut().unwrap();
        let path_field = handle.field("path").unwrap();
        let content_field = handle.field("content").unwrap();
        
        let mut doc = ld_lucivy::LucivyDocument::new();
        doc.add_text(path_field, "test.rs");
        doc.add_text(content_field, "use rag3weaver for search");
        writer.add_document(doc).unwrap();
        writer.commit().unwrap();
    }
    handle.reader.reload().unwrap();
    
    let searcher = handle.reader.searcher();
    let content_field = handle.field("content").unwrap();
    
    for (i, reader) in searcher.segment_readers().iter().enumerate() {
        let has_sfx = reader.sfx_file(content_field).is_some();
        let has_posmap = reader.posmap_file(content_field).is_some();
        let has_registry_posmap = reader.sfx_index_file("posmap", content_field).is_some();
        let has_termtexts = reader.sfx_index_file("termtexts", content_field).is_some();
        let sfx_ids = reader.segment_id();
        eprintln!("seg[{}] {:?}: sfx={} posmap={} posmap_reg={} termtexts={}",
            i, sfx_ids, has_sfx, has_posmap, has_registry_posmap, has_termtexts);
    }
    
    // Try fuzzy search
    let sink = std::sync::Arc::new(ld_lucivy::query::HighlightSink::new());
    let qconfig = QueryConfig {
        query_type: "contains".into(),
        field: Some("content".into()),
        value: Some("rak3weaver".to_string()),
        distance: Some(1),
        ..Default::default()
    };
    let q = query::build_query(&qconfig, &handle.schema, &handle.index, Some(sink.clone())).unwrap();
    let collector = ld_lucivy::collector::TopDocs::with_limit(100).order_by_score();
    let results = searcher.search(&*q, &collector).unwrap();
    eprintln!("fuzzy 'rak3weaver' d=1: {} results", results.len());
    assert!(results.len() > 0, "should find rag3weaver");
}

#[test]
fn test_multi_segment_fuzzy() {
    let tmp = std::path::Path::new("/tmp/test_multi_seg_fuzzy");
    let _ = std::fs::remove_dir_all(tmp);
    std::fs::create_dir_all(tmp).unwrap();
    
    let config = SchemaConfig {
        fields: vec![
            query::FieldDef { name: "content".into(), field_type: "text".into(), stored: Some(true), indexed: Some(true), fast: None },
        ],
        ..Default::default()
    };
    let dir = StdFsDirectory::open(tmp).unwrap();
    let handle = LucivyHandle::create(dir, &config).unwrap();
    
    {
        let mut guard = handle.writer.lock().unwrap();
        let writer = guard.as_mut().unwrap();
        let f = handle.field("content").unwrap();
        
        let mut doc = ld_lucivy::LucivyDocument::new();
        doc.add_text(f, "use rag3weaver for search");
        writer.add_document(doc).unwrap();
        writer.commit().unwrap();
    }
    handle.reader.reload().unwrap();
    let searcher = handle.reader.searcher();
    
    let queries: Vec<(&str, u8, &str)> = vec![
        // Exact (d=0) multi-token
        ("use rag3weaver", 0, "d=0 2 segments"),
        ("use rag3weaver for", 0, "d=0 3 segments"),
        ("use rag3weaver for search", 0, "d=0 4 segments"),
        ("rag3weaver for search", 0, "d=0 3 segments no prefix"),
        // Fuzzy (d=1) multi-token
        ("use rak3weaver", 1, "d=1 2 segments"),
        ("use rak3weaver for", 1, "d=1 3 segments"),
        ("use rak3weaver for search", 1, "d=1 4 segments"),
        ("rak3weaver for search", 1, "d=1 3 segments no prefix"),
    ];
    
    for (q, d, desc) in &queries {
        let sink = std::sync::Arc::new(ld_lucivy::query::HighlightSink::new());
        let qconfig = QueryConfig {
            query_type: "contains".into(),
            field: Some("content".into()),
            value: Some(q.to_string()),
            distance: Some(*d),
            ..Default::default()
        };
        let query = query::build_query(&qconfig, &handle.schema, &handle.index, Some(sink.clone())).unwrap();
        let collector = ld_lucivy::collector::TopDocs::with_limit(100).order_by_score();
        let results = searcher.search(&*query, &collector).unwrap();
        eprintln!("fuzzy '{}' d={} ({}): {} results", q, d, desc, results.len());
        assert!(results.len() > 0, "'{}' d={} ({}) should find results", q, d, desc);
    }
}
