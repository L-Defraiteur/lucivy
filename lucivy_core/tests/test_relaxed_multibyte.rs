//! Relaxed separators against non-ASCII text: `de` on the whole kernel
//! (13 September 2026, night) rendered 6 spans the ground truth does not
//! have, every one a `d`, separators, then a multi-byte character —
//! `D\n,,“underscan`, `` d`` 文件 ``, `d\n\n取消`. No `e` anywhere in the span.

use std::sync::Arc;

use lucivy_core::handle::{LucivyHandle, NODE_ID_FIELD};
use lucivy_core::query::{self, QueryConfig, SchemaConfig};

const DOCS: [&str; 7] = [
    "al\"\" }\",Connector,TB\nD\n,,“underscan”,ENUM,",
    "使用\n  ``delete_child`` 文件::\n\n    echo 0x",
    "ces/DEVICE/authorized\n\n取消对设备的授",
    "plain de text",
    "d é e",
    // The same shapes in ASCII: what must keep working.
    "al\"\" }\",Connector,TB\nD\n,,underscan,ENUM,",
    "d, eagle and d ,underscan",
];

fn build(sfx_version: u8, positions: bool) -> LucivyHandle {
    let config: SchemaConfig = serde_json::from_value(serde_json::json!({
        "fields": [{"name": "content", "type": "text", "stored": true}],
        "sfx_version": sfx_version,
        "positions": positions
    })).unwrap();
    let handle = LucivyHandle::create(ld_lucivy::directory::RamDirectory::default(), &config).unwrap();
    let content_f = handle.field("content").unwrap();
    let nid_f = handle.field(NODE_ID_FIELD).unwrap();
    {
        let mut guard = handle.writer.lock().unwrap();
        let w = guard.as_mut().unwrap();
        for (i, text) in DOCS.iter().enumerate() {
            let mut doc = ld_lucivy::LucivyDocument::new();
            doc.add_u64(nid_f, i as u64);
            doc.add_text(content_f, text);
            w.add_document(doc).unwrap();
        }
        w.commit().unwrap();
    }
    handle.reader.reload().unwrap();
    handle
}

fn search(handle: &LucivyHandle, value: &str, strict: bool) -> Vec<(u64, Vec<(usize, usize)>)> {
    let sink = Arc::new(ld_lucivy::query::HighlightSink::new());
    let cfg = QueryConfig {
        query_type: "contains".into(),
        field: Some("content".into()),
        value: Some(value.into()),
        strict_separators: Some(strict),
        ..Default::default()
    };
    let query = query::build_query(&cfg, &handle.schema, &handle.index, Some(Arc::clone(&sink))).unwrap();
    let searcher = handle.reader.searcher();
    let collector = ld_lucivy::collector::TopDocs::with_limit(100).order_by_score();
    let nid_f = handle.field(NODE_ID_FIELD).unwrap();
    let mut out = Vec::new();
    for (_score, addr) in searcher.search(&*query, &collector).unwrap() {
        let doc = searcher.doc::<ld_lucivy::LucivyDocument>(addr).unwrap();
        use ld_lucivy::schema::document::Value;
        let nid = doc.field_values().find(|(f, _)| *f == nid_f)
            .and_then(|(_, v)| v.as_value().as_u64()).unwrap();
        let seg_id = searcher.segment_reader(addr.segment_ord).segment_id();
        let mut spans: Vec<(usize, usize)> = sink.get(seg_id, addr.doc_id)
            .and_then(|hl| hl.get("content").cloned())
            .unwrap_or_default()
            .into_iter().map(|[s, e]| (s, e)).collect();
        spans.sort();
        out.push((nid, spans));
    }
    out.sort_by_key(|r| r.0);
    out
}

/// What the ground truth says: relaxed `de` is `d`, any run of separators
/// (bytes that are neither letters nor digits, multi-byte ones included),
/// then `e`.
fn truth(text: &str) -> Vec<(usize, usize)> {
    let b = text.as_bytes();
    let lower: Vec<u8> = b.iter().map(|c| c.to_ascii_lowercase()).collect();
    let mut out = Vec::new();
    for i in 0..lower.len() {
        if lower[i] != b'd' { continue; }
        let mut j = i + 1;
        let s = text;
        while j < s.len() {
            let ch = s[j..].chars().next().unwrap();
            if ch.is_alphanumeric() { break; }
            j += ch.len_utf8();
        }
        if j < lower.len() && lower[j] == b'e' {
            out.push((i, j + 1));
        }
    }
    out
}

#[test]
fn relaxed_needle_does_not_end_inside_a_multibyte_character() {
    for sfx_version in [3u8, 4] {
        for positions in [true, false] {
            let what = format!("sfx_version {sfx_version}, positions {positions}");
            let h = build(sfx_version, positions);
            let got = search(&h, "de", false);
            for (i, text) in DOCS.iter().enumerate() {
                let want = truth(text);
                let have = got.iter().find(|r| r.0 == i as u64).map(|r| r.1.clone()).unwrap_or_default();
                assert_eq!(have, want, "{what}: doc {i} {text:?}");
            }
            let strict = search(&h, "de", true);
            assert_eq!(strict.iter().map(|r| r.0).collect::<Vec<_>>(), vec![0, 1, 2, 3, 5, 6], "{what}: strict `de` — `underscan` and `DEVICE` hold it too");
        }
    }
}
