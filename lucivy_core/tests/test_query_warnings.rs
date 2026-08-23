//! End-to-end: warnings through a real handle (query + index facts).

use lucivy_core::query::QueryConfig;
use lucivy_core::handle::LucivyHandle;

#[test]
fn warnings_through_handle() {
    let config: lucivy_core::query::SchemaConfig = serde_json::from_value(serde_json::json!({
        "fields": [{"name": "body", "type": "text", "stored": true}],
        "sfx_version": 3
    })).unwrap();
    let handle = LucivyHandle::create(ld_lucivy::directory::RamDirectory::default(), &config).unwrap();

    let q = |t: &str, v: &str| QueryConfig {
        query_type: t.into(), field: Some("body".into()), value: Some(v.into()), ..Default::default()
    };
    assert!(handle.query_warnings(&q("contains", "kmalloc")).is_empty());
    let w = handle.query_warnings(&q("contains", "__init"));
    assert_eq!(w.len(), 1, "{w:?}");
    let w = handle.query_warnings(&q("regex", "[0-9]{8}"));
    assert_eq!(w.len(), 1, "{w:?}");
    for (t, v) in [("contains", "__init"), ("fuzzy", "init"), ("regex", "[0-9]{8}"), ("regex", r"/\*[^*]*\*/"), ("contains", "->")] {
        for m in handle.query_warnings(&q(t, v)) {
            eprintln!("{t} {v:?}: {m}");
        }
    }
}
