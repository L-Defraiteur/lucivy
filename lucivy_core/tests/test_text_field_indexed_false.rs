//! Unsupported text indexing options must be reported, without changing indexing.
use lucivy_core::handle::schema_warnings;
use lucivy_core::query::SchemaConfig;

#[test]
fn unsupported_text_indexing_options_are_reported_once_per_field() {
    let config: SchemaConfig = serde_json::from_value(serde_json::json!({
        "fields": [
            {"name": "body", "type": "text", "indexed": false},
            {"name": "label", "type": "string", "indexed": false},
            {"name": "enabled", "type": "text", "indexed": true},
            {"name": "default_text", "type": "text"},
            {"name": "default_string", "type": "string"},
            {"name": "count", "type": "u64", "indexed": false},
            {"name": "signed", "type": "i64", "indexed": false},
            {"name": "ratio", "type": "f64", "indexed": false}
        ]
    })).unwrap();
    assert_eq!(schema_warnings(&config), vec![
        "field \"body\": indexed=false is not supported for text and string fields; the field is indexed",
        "field \"label\": indexed=false is not supported for text and string fields; the field is indexed",
    ]);
}
