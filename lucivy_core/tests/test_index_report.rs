//! `index_report`: what an index is made of, on an index whose numbers are
//! known — sizes 10 to 100 bytes, three copies of one content, two deletions.

mod common;

use lucivy_core::query::SchemaConfig;
use lucivy_core::sharded_handle::ShardedHandle;

const DUPLICATE: &str = "the same content in three documents";

/// Nearest-rank percentile on ascending sizes, as `index_report` computes it.
fn percentile(sorted: &[u64], p: f64) -> u64 {
    let rank = ((p * sorted.len() as f64).ceil() as usize).max(1);
    sorted[rank - 1]
}

#[test]
fn the_report_gives_the_known_numbers_of_a_fixture() {
    let scratch = common::scratch("index-report");
    let config: SchemaConfig = serde_json::from_value(serde_json::json!({
        "fields": [{ "name": "content", "type": "text" }],
        "shards": 1
    })).unwrap();
    let index = ShardedHandle::create(scratch.path().to_str().unwrap(), &config).unwrap();
    // Nodes 0..9: 10, 20, … 100 bytes. Nodes 10..12: the same content.
    for i in 0..10u64 {
        index.add_document_json(i, &serde_json::json!({ "content": "a".repeat(10 * (i as usize + 1)) })).unwrap();
    }
    for i in 10..13u64 {
        index.add_document_json(i, &serde_json::json!({ "content": DUPLICATE })).unwrap();
    }
    index.commit().unwrap();
    index.delete_by_node_id(0).unwrap();
    index.delete_by_node_id(1).unwrap();
    index.commit().unwrap();
    index.wait_merges_quiet().unwrap();

    let report = index.index_report().unwrap();
    eprintln!("{}", report.summary());

    // Segments: 13 documents written, 2 deleted.
    assert_eq!(report.segments.iter().map(|s| s.max_doc as u64).sum::<u64>(), 13);
    assert_eq!(report.segments.iter().map(|s| s.deleted as u64).sum::<u64>(), 2);
    assert!(report.segments.iter().all(|s| s.bytes > 0));

    // The content field, over the 11 live documents.
    let d = DUPLICATE.len() as u64;
    let mut sizes: Vec<u64> = (3..=10).map(|k| 10 * k).collect();
    sizes.extend([d, d, d]);
    sizes.sort_unstable();
    let field = report.fields.iter().find(|f| f.field == "content").expect("content");
    assert_eq!(field.docs, 11);
    assert_eq!(field.bytes, 520 + 3 * d);
    assert_eq!(field.distinct, 9);
    assert_eq!(field.duplicate_docs, 2);
    assert_eq!(field.duplicate_bytes, 2 * d);
    assert_eq!(field.p50, percentile(&sizes, 0.5));
    assert_eq!(field.p90, percentile(&sizes, 0.9));
    assert_eq!(field.p99, percentile(&sizes, 0.99));
    assert_eq!(field.max, 100);
    assert_eq!(field.largest.first(), Some(&(9, 100)), "the largest by _node_id");
    assert_eq!(field.largest.len(), 10);
    assert!(field.largest.iter().all(|(node, _)| *node >= 2), "deleted documents are not counted");

    // The files: every family is there, and the ratio is index / stored text.
    for kind in ["stored text", "positions", "dictionary", "postings"] {
        assert!(report.bytes_by_kind.get(kind).is_some_and(|b| *b > 0), "{kind}: {:?}", report.bytes_by_kind);
    }
    assert_eq!(report.index_bytes, report.bytes_by_kind.values().sum::<u64>());
    assert_eq!(report.stored_text_bytes, field.bytes);
    assert!((report.ratio() - report.index_bytes as f64 / report.stored_text_bytes as f64).abs() < 1e-9);
    index.close().unwrap();
}
