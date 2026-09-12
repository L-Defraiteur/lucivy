//! One occurrence, one span. In relaxed mode a needle that ends a word cut
//! into chunks (`lock` in `superblock`, chunks `super` + `block`) is found
//! twice by the literal phase — through the word's entry, at the position
//! of its first chunk, and through the last chunk — with the same bytes.
//! Up to 4.0.2 both survived the deduplication, keyed by position: the span
//! came back twice and counted twice in the term frequency (542 duplicated
//! spans for `lock` over 10 000 kernel files). The index without positions,
//! which verifies every match on the stored text, never had it: its scores
//! are the reference here.
//!
//! Also checked: two real occurrences inside one token (`initinit`) both
//! stay, strict mode is unchanged, and the same holds for long words in
//! multibyte text — accents, a case fold on two-byte characters, an emoji
//! between two words, CJK — where the spans are byte offsets of the source.

use std::sync::Arc;

use lucivy_core::handle::{LucivyHandle, NODE_ID_FIELD};
use lucivy_core::query::{self, QueryConfig, SchemaConfig};

const DOCS: [&str; 9] = [
    "superblock",
    "aaaaalock",
    "initinit",
    "the superblock and the superblock",
    "a clock on the wall",
    "déjàsuperblock",
    "ÉTÉsuperblock",
    "superblock🙂superblock",
    "可以理解可以理解可以理解",
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

/// Per document: its score and every span, duplicates kept, sorted.
fn search(handle: &LucivyHandle, value: &str, strict: bool) -> Vec<(u64, f32, Vec<(usize, usize)>)> {
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
    for (score, addr) in searcher.search(&*query, &collector).unwrap() {
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
        out.push((nid, score, spans));
    }
    out.sort_by_key(|r| r.0);
    out
}

fn spans_of(results: &[(u64, f32, Vec<(usize, usize)>)], nid: u64) -> Vec<(usize, usize)> {
    results.iter().find(|r| r.0 == nid).map(|r| r.2.clone()).unwrap_or_default()
}

#[test]
fn one_occurrence_is_one_span_in_every_layout() {
    for sfx_version in [3u8, 4] {
        for positions in [true, false] {
            let what = format!("sfx_version {sfx_version}, positions {positions}");
            let h = build(sfx_version, positions);

            let lock = search(&h, "lock", false);
            assert_eq!(spans_of(&lock, 0), vec![(6, 10)], "{what}: `lock` in `superblock`");
            assert_eq!(spans_of(&lock, 1), vec![(5, 9)], "{what}: `lock` in `aaaaalock`");
            assert_eq!(spans_of(&lock, 3), vec![(10, 14), (29, 33)], "{what}: two `superblock`");
            assert_eq!(spans_of(&lock, 4), vec![(3, 7)], "{what}: `clock`");

            let block = search(&h, "block", false);
            assert_eq!(spans_of(&block, 0), vec![(5, 10)], "{what}: `block` in `superblock`");

            let init = search(&h, "init", false);
            assert_eq!(spans_of(&init, 2), vec![(0, 4), (4, 8)], "{what}: two occurrences in one token stay");

            let strict = search(&h, "lock", true);
            assert_eq!(spans_of(&strict, 0), vec![(6, 10)], "{what}: strict");

            // Multibyte: `déjà` is 6 bytes, `ÉTÉ` 5, the emoji 4, a CJK character 3.
            assert_eq!(spans_of(&lock, 5), vec![(12, 16)], "{what}: `lock` after `déjà`");
            assert_eq!(spans_of(&lock, 6), vec![(11, 15)], "{what}: `lock` after `ÉTÉ`");
            assert_eq!(spans_of(&lock, 7), vec![(6, 10), (20, 24)], "{what}: `lock` around an emoji");
            let ete = search(&h, "été", false);
            assert_eq!(spans_of(&ete, 6), vec![(0, 5)], "{what}: `été` folds `ÉTÉ`");
            let cjk = search(&h, "理解", false);
            assert_eq!(spans_of(&cjk, 8), vec![(6, 12), (18, 24), (30, 36)], "{what}: CJK, one long word");
        }
    }
}

#[test]
fn the_default_index_scores_like_the_index_that_reads_the_text() {
    for sfx_version in [3u8, 4] {
        let with = build(sfx_version, true);
        let without = build(sfx_version, false);
        for (value, strict) in [("lock", false), ("block", false), ("init", false), ("lock", true),
                                ("été", false), ("理解", false), ("superblock", false)] {
            let a = search(&with, value, strict);
            let b = search(&without, value, strict);
            assert_eq!(a.len(), b.len(), "sfx_version {sfx_version}, {value}: same documents");
            for (x, y) in a.iter().zip(&b) {
                assert_eq!(x.0, y.0, "sfx_version {sfx_version}, {value}: same documents");
                assert_eq!(x.2, y.2, "sfx_version {sfx_version}, {value}: same spans for document {}", x.0);
                assert!((x.1 - y.1).abs() < 1e-4,
                        "sfx_version {sfx_version}, {value}: document {} scores {} with positions, {} without", x.0, x.1, y.1);
            }
        }
    }
}
