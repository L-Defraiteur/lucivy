//! A v4 binary opens an index written by the published **3.0.8** and answers
//! exactly what 3.0.8 answered — documents and spans, on a panel of query
//! kinds, one shard and two shards — then converts it on its first commit
//! without losing a document or a span, and the reopened index still agrees.
//! The fixture (`tests/fixtures/index-3.0.8/`) was built by the PyPI wheel
//! over 14 kernel files and four synthetic documents; `panel-3.0.8.json`
//! holds the wheel's own answers. This is the contract behind the major
//! version: 4.0 reads 3.0.x, the first commit in 4.0 converts, 3.0.x does
//! not read 4.0.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use lucivy_core::query::QueryConfig;
use lucivy_core::sharded_handle::ShardedHandle;

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/index-3.0.8")
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap().flatten() {
        let p = e.path();
        let dest = to.join(e.file_name());
        if p.is_dir() { copy_dir(&p, &dest); } else { std::fs::copy(&p, &dest).unwrap(); }
    }
}

type Docs = BTreeSet<u64>;
type Spans = BTreeSet<(u64, usize, usize)>;

fn run(h: &ShardedHandle, q: &QueryConfig) -> (Docs, Spans) {
    use ld_lucivy::schema::document::Value;
    let nid_f = h.field("_node_id").unwrap();
    let hits = h.search_with_docs(q, 100_000).unwrap();
    let mut docs = Docs::new();
    let mut spans = Spans::new();
    for hit in hits {
        let nid = hit.doc.field_values().find(|(f, _)| *f == nid_f)
            .and_then(|(_, v)| v.as_value().as_u64()).expect("node id");
        docs.insert(nid);
        for [s, e] in hit.highlights.get("content").cloned().unwrap_or_default() {
            spans.insert((nid, s, e));
        }
    }
    (docs, spans)
}

struct Entry { label: String, query: QueryConfig, docs: Docs, spans: Spans, value: String }

fn panel(name: &str) -> Vec<Entry> {
    let json: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(fixture_root().join("panel-3.0.8.json")).unwrap()).unwrap();
    json["panel"][name].as_array().unwrap().iter().map(|e| Entry {
        label: e["label"].as_str().unwrap().to_string(),
        value: e["query"]["value"].as_str().unwrap().to_string(),
        query: serde_json::from_value(e["query"].clone()).unwrap(),
        docs: e["docs"].as_array().unwrap().iter().map(|d| d.as_u64().unwrap()).collect(),
        spans: e["spans"].as_array().unwrap().iter()
            .map(|s| (s[0].as_u64().unwrap(), s[1].as_u64().unwrap() as usize, s[2].as_u64().unwrap() as usize)).collect(),
    }).collect()
}

/// The magics of the segment files under `dir`, by extension.
fn magics(dir: &Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let ext = name.rsplit('.').next().unwrap_or("").to_string();
        if !matches!(ext.as_str(), "sfxpost" | "word_sfxpost" | "posmap") { continue; }
        let bytes = std::fs::read(e.path()).unwrap();
        out.push((ext, String::from_utf8_lossy(&bytes[..4.min(bytes.len())]).to_string()));
    }
    out.sort();
    out
}

fn check(name: &str) {
    let base: PathBuf = std::env::temp_dir().join(format!("lucivy-compat-308-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    copy_dir(&fixture_root().join(name), &base);
    let shard_dirs: Vec<PathBuf> = std::fs::read_dir(&base).unwrap().flatten()
        .map(|e| e.path()).filter(|p| p.is_dir()).collect();
    for sd in &shard_dirs {
        let meta = std::fs::read_to_string(sd.join("meta.json")).unwrap().replace(char::is_whitespace, "");
        assert!(meta.contains("\"sfx_version\":3"), "{name}: a 3.0.8 shard is sfx_version 3: {meta}");
        let m = magics(sd);
        assert!(m.iter().all(|(ext, magic)| matches!((ext.as_str(), magic.as_str()),
            ("sfxpost", "SFP3") | ("word_sfxpost", "WSP3") | ("posmap", "PMP3") | ("posmap", "PMAP"))),
            "{name}: 3.0.8 layouts expected, got {m:?}");
    }

    // 1. Read: what 3.0.8 answered, to the span.
    let h = ShardedHandle::open(base.to_str().unwrap()).unwrap();
    let entries = panel(name);
    let mut before = Vec::new();
    assert!(entries.iter().filter(|e| !e.docs.is_empty()).count() >= 12, "{name}: the 3.0.8 panel found something on most queries");
    for e in &entries {
        let (docs, spans) = run(&h, &e.query);
        assert_eq!(docs, e.docs, "{name}: {} {}: documents differ from 3.0.8", e.value, e.label);
        assert_eq!(spans, e.spans, "{name}: {} {}: spans differ from 3.0.8", e.value, e.label);
        eprintln!("{name:<8} {:<18} {:<10} {:>3} docs {:>4} spans — as 3.0.8", e.value, e.label, docs.len(), spans.len());
        before.push((docs, spans));
    }

    // 2. Convert: new documents, a commit, the merges — the first v4 write
    // into a 3.0.8 index. The old answers must survive whole; the new
    // documents must be found with exact spans.
    let marker = "fixture_marker_v4";
    let mut new_ids = Docs::new();
    for i in 0..6u64 {
        let nid = 1000 + i;
        let content = format!("{}\nnew document {i}: mutex_lock(&m); {marker} here; return -ENOMEM;\n{}", "x".repeat(i as usize * 7), "y".repeat(i as usize * 3));
        h.add_document_json(nid, &serde_json::json!({"path": format!("new/{i}.c"), "content": content})).unwrap();
        new_ids.insert(nid);
    }
    h.commit().unwrap();
    h.wait_merges_quiet().unwrap();
    h.commit().unwrap();
    let mut after = Vec::new();
    for (e, (docs0, spans0)) in entries.iter().zip(&before) {
        let (docs, spans) = run(&h, &e.query);
        assert!(docs0.is_subset(&docs), "{name}: {} {}: a 3.0.8 document is lost after the conversion", e.value, e.label);
        assert!(spans0.is_subset(&spans), "{name}: {} {}: a 3.0.8 span is lost after the conversion", e.value, e.label);
        let added: Docs = docs.difference(docs0).copied().collect();
        assert!(added.is_subset(&new_ids), "{name}: {} {}: only the new documents may be added: {added:?}", e.value, e.label);
        after.push((docs, spans));
    }
    let marker_q: QueryConfig = serde_json::from_value(serde_json::json!(
        {"type": "contains", "field": "content", "value": marker, "strict_separators": true})).unwrap();
    let (docs, spans) = run(&h, &marker_q);
    assert_eq!(docs, new_ids, "{name}: every new document carries the marker");
    let expected: Spans = (0..6u64).map(|i| {
        let head = format!("{}\nnew document {i}: mutex_lock(&m); ", "x".repeat(i as usize * 7));
        (1000 + i, head.len(), head.len() + marker.len())
    }).collect();
    assert_eq!(spans, expected, "{name}: the marker's spans are exact");
    let mutex_q: QueryConfig = serde_json::from_value(serde_json::json!(
        {"type": "contains", "field": "content", "value": "mutex_lock", "strict_separators": true})).unwrap();
    let (docs, _) = run(&h, &mutex_q);
    assert!(new_ids.is_subset(&docs) && entries[0].docs.is_subset(&docs), "{name}: mutex_lock finds old and new documents");

    // Something was written in the current layouts.
    let m: Vec<(String, String)> = shard_dirs.iter().flat_map(|sd| magics(sd)).collect();
    assert!(m.iter().any(|(ext, magic)| ext == "sfxpost" && magic == "SFP5"), "{name}: a v4 segment was written: {m:?}");
    let old_left = m.iter().filter(|(ext, magic)| ext == "sfxpost" && magic == "SFP3").count();
    eprintln!("{name}: after the conversion, {} sfxpost files, {old_left} still in the 3.0.8 layout", m.iter().filter(|(e, _)| e == "sfxpost").count());

    // Compact: the 3.0.8 segments merge with the v4 one — the merge reads
    // the old layouts (spans in the postings, `PMP3`) and writes the new
    // (`SFP5`, `WSP5`, `PMP4`, tail offsets converted). Nothing may move.
    h.compact(1_000_000).unwrap();
    h.wait_merges_quiet().unwrap();
    let m: Vec<(String, String)> = shard_dirs.iter().flat_map(|sd| magics(sd)).collect();
    assert!(m.iter().all(|(_, magic)| matches!(magic.as_str(), "SFP5" | "WSP5" | "PMP4")),
        "{name}: after compaction every segment is in the current layouts: {m:?}");
    for (e, (docs, spans)) in entries.iter().zip(&after) {
        let (d, s) = run(&h, &e.query);
        assert_eq!(&d, docs, "{name}: {} {} after compaction: documents differ", e.value, e.label);
        assert_eq!(&s, spans, "{name}: {} {} after compaction: spans differ", e.value, e.label);
    }
    let (d, s) = run(&h, &marker_q);
    assert_eq!((d, s), (new_ids.clone(), expected.clone()), "{name}: the marker after compaction");
    eprintln!("{name}: compacted — {} sfxpost files, all current layouts, same answers", m.iter().filter(|(e, _)| e == "sfxpost").count());

    // 3. Reopen: the converted index answers the same.
    h.close().unwrap();
    let h2 = ShardedHandle::open(base.to_str().unwrap()).unwrap();
    for (e, (docs, spans)) in entries.iter().zip(&after) {
        let (d, s) = run(&h2, &e.query);
        assert_eq!(&d, docs, "{name}: {} {} after reopen: documents differ", e.value, e.label);
        assert_eq!(&s, spans, "{name}: {} {} after reopen: spans differ", e.value, e.label);
    }
    let (d, s) = run(&h2, &marker_q);
    assert_eq!((d, s), (new_ids, expected), "{name}: the marker after reopen");
    h2.close().unwrap();
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn a_308_index_answers_like_308_then_converts_without_loss() {
    check("single");
    check("sharded");
}
