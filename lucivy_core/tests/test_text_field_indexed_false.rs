//! Unsupported text indexing options must be reported, without changing indexing.
use lucivy_core::handle::{build_schema, schema_warnings};
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
        r#"field "body": indexed=false is not supported for text and string fields; the field is indexed"#,
        r#"field "label": indexed=false is not supported for text and string fields; the field is indexed"#,
    ]);
}

#[test]
fn warning_escapes_field_names_without_extra_lines() {
    let config: SchemaConfig = serde_json::from_value(serde_json::json!({
        "fields": [{"name": "line\n\"quoted", "type": "text", "indexed": false}]
    })).unwrap();
    let warnings = schema_warnings(&config);
    assert_eq!(warnings.len(), 1);
    assert!(!warnings[0].contains('\n'));
    assert!(warnings[0].contains(r#"line\n\"quoted"#));
}

#[test]
fn reporting_preserves_existing_field_indexing() {
    let config: SchemaConfig = serde_json::from_value(serde_json::json!({
        "fields": [
            {"name": "body", "type": "text", "indexed": false},
            {"name": "label", "type": "string", "indexed": false},
            {"name": "count", "type": "u64", "indexed": false}
        ]
    })).unwrap();
    let (schema, _) = build_schema(&config).unwrap();
    for name in ["body", "label"] {
        let field = schema.get_field(name).unwrap();
        assert!(schema.get_field_entry(field).is_indexed());
    }
    let field = schema.get_field("count").unwrap();
    assert!(!schema.get_field_entry(field).is_indexed());
}
