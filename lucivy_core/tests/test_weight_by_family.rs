//! What a family of documents weighs in a real index, and what removing it would give back: the stored
//! documents of a COPY grouped by a family given per `_node_id` (a file's extension, its top folder, the
//! size of its content), then, for the largest families, the index rebuilt without them and compacted,
//! against the index rebuilt with everything. The difference is what an exclusion rule would save.
//!
//!     LUCIVY_FAMILY_INDEX=<copy of an index> \
//!     LUCIVY_FAMILY_MAP=<tsv: _node_id, path, …> \
//!     LUCIVY_FAMILY_OUT=<empty directory on disk, not /tmp> \
//!     [LUCIVY_FAMILY_TOP=6] \
//!     cargo test --release -p lucivy-core --test test_weight_by_family -- --ignored --nocapture

use std::collections::{BTreeMap, HashMap};

use ld_lucivy::schema::OwnedValue;
use ld_lucivy::Document;
use lucivy_core::handle::NODE_ID_FIELD;
use lucivy_core::maintenance::MaintenanceRequest;
use lucivy_core::query::SchemaConfig;
use lucivy_core::sharded_handle::ShardedHandle;

struct Doc {
    node: u64,
    fields: serde_json::Value,
    text_bytes: u64,
    content_bytes: u64,
}

fn stored_documents(source: &ShardedHandle) -> Vec<Doc> {
    let mut out = Vec::new();
    for shard in 0..source.num_shards() {
        let handle = source.shard(shard).unwrap();
        let node_field = handle.field(NODE_ID_FIELD).unwrap();
        let fields: Vec<(String, ld_lucivy::schema::Field)> =
            handle.field_map.iter().filter(|(n, _)| n != NODE_ID_FIELD).cloned().collect();
        let searcher = handle.reader.searcher();
        for segment in searcher.segment_readers() {
            let store = segment.get_store_reader(64).unwrap();
            let alive = segment.alive_bitset();
            for doc_id in 0..segment.max_doc() {
                if alive.is_some_and(|a| !a.is_alive(doc_id)) { continue }
                let doc: ld_lucivy::LucivyDocument = store.get(doc_id).unwrap();
                let (mut node, mut object, mut text_bytes, mut content_bytes) = (None, serde_json::Map::new(), 0, 0);
                for (field, value) in doc.iter_fields_and_values() {
                    let owned: OwnedValue = value.into();
                    if field == node_field {
                        if let OwnedValue::U64(n) = owned { node = Some(n) }
                        continue;
                    }
                    let Some((name, _)) = fields.iter().find(|(_, f)| *f == field) else { continue };
                    if let OwnedValue::Str(s) = owned {
                        if !name.starts_with('_') { text_bytes += s.len() as u64 }
                        if name == "content" { content_bytes += s.len() as u64 }
                        object.insert(name.clone(), serde_json::Value::String(s));
                    }
                }
                out.push(Doc { node: node.expect("_node_id"), fields: serde_json::Value::Object(object), text_bytes, content_bytes });
            }
        }
    }
    out
}

fn extension(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => format!(".{}", ext.to_lowercase()),
        _ => "(sans extension)".into(),
    }
}

fn top_folder(path: &str) -> String {
    match path.split_once('/') { Some((top, _)) => format!("{top}/"), None => "(racine)".into() }
}

fn size_band(bytes: u64) -> String {
    let bands = [(1_000, "≤ 1 Ko"), (10_000, "1-10 Ko"), (100_000, "10-100 Ko"), (1_000_000, "100 Ko-1 Mo")];
    bands.iter().find(|(lim, _)| bytes <= *lim).map(|(_, l)| l.to_string()).unwrap_or_else(|| "> 1 Mo".into())
}

fn mb(b: u64) -> String { format!("{:.1} Mo", b as f64 / 1e6) }

/// The index of `docs` (those `keep` accepts), compacted: (index bytes, dictionary bytes).
fn rebuilt(config: &SchemaConfig, docs: &[Doc], keep: impl Fn(&Doc) -> bool, path: &std::path::Path) -> (u64, u64) {
    let index = ShardedHandle::create(path.to_str().unwrap(), config).unwrap();
    for (n, d) in docs.iter().filter(|d| keep(d)).enumerate() {
        index.add_document_json(d.node, &d.fields).unwrap();
        if (n + 1) % 20_000 == 0 { index.commit().unwrap() }
    }
    index.commit().unwrap();
    let report = index.maintain(&MaintenanceRequest { compact: true, ..Default::default() }).unwrap();
    index.close().unwrap();
    std::fs::remove_dir_all(path).unwrap();
    (report.after.index_bytes, *report.after.bytes_by_kind.get("dictionary").unwrap_or(&0))
}

#[test]
#[ignore]
fn weight_by_family() {
    let source_path = std::env::var("LUCIVY_FAMILY_INDEX").expect("LUCIVY_FAMILY_INDEX");
    let out = std::path::PathBuf::from(std::env::var("LUCIVY_FAMILY_OUT").expect("LUCIVY_FAMILY_OUT"));
    assert!(!out.to_string_lossy().starts_with("/tmp"), "LUCIVY_FAMILY_OUT: on disk, not /tmp");
    let top: usize = std::env::var("LUCIVY_FAMILY_TOP").map(|v| v.parse().unwrap()).unwrap_or(6);
    let paths: HashMap<u64, String> = std::fs::read_to_string(std::env::var("LUCIVY_FAMILY_MAP").expect("LUCIVY_FAMILY_MAP"))
        .unwrap().lines()
        .filter_map(|l| { let mut c = l.split('\t'); Some((c.next()?.parse().ok()?, c.next()?.to_string())) })
        .collect();
    let config: SchemaConfig = serde_json::from_str(&std::fs::read_to_string(format!("{source_path}/_shard_config.json")).unwrap()).unwrap();
    let source = ShardedHandle::open(&source_path).unwrap();
    let report = source.index_report().unwrap();
    println!("== the index as it is\n{}", report.summary());
    let docs = stored_documents(&source);
    drop(source);
    let unmapped = docs.iter().filter(|d| !paths.contains_key(&d.node)).count();
    let text: u64 = docs.iter().map(|d| d.text_bytes).sum();
    println!("{} live documents, {} of text (content, name, docstring) ; {} without a Scope in the graph ; {} Scope rows",
        docs.len(), mb(text), unmapped, paths.len());

    type Key = fn(&str, u64) -> String;
    let kinds: [(&str, Key); 3] = [
        ("extension", |p, _| extension(p)),
        ("top folder", |p, _| top_folder(p)),
        ("content size", |_, b| size_band(b)),
    ];
    let family_of = |kind: Key, d: &Doc| paths.get(&d.node).map(|p| kind(p, d.content_bytes)).unwrap_or_else(|| "(sans Scope)".into());
    std::fs::create_dir_all(&out).unwrap();
    let (all_index, all_dict) = rebuilt(&config, &docs, |_| true, &out.join("all"));
    println!("\n== rebuilt with everything and compacted: {} (dictionary {})", mb(all_index), mb(all_dict));
    for (kind_name, kind) in kinds {
        let mut by: BTreeMap<String, (u64, u64)> = BTreeMap::new();
        for d in &docs {
            let e = by.entry(family_of(kind, d)).or_default();
            e.0 += 1;
            e.1 += d.text_bytes;
        }
        let mut rows: Vec<(String, u64, u64)> = by.into_iter().map(|(k, (n, b))| (k, n, b)).collect();
        rows.sort_by(|a, b| b.2.cmp(&a.2));
        println!("\n== by {kind_name}: family | documents | text | share of the text");
        for (k, n, b) in rows.iter().take(20) {
            println!("{k}\t{n}\t{}\t{:.1} %", mb(*b), 100.0 * *b as f64 / text.max(1) as f64);
        }
        if kind_name == "content size" { continue }
        println!("-- the index without each of the {top} largest {kind_name} families (rebuilt, compacted): saved | dictionary saved");
        for (k, _, _) in rows.iter().take(top) {
            let (index, dict) = rebuilt(&config, &docs, |d| &family_of(kind, d) != k, &out.join("without"));
            println!("without {k}\t{} ({:.1} %)\t{}", mb(all_index.saturating_sub(index)),
                100.0 * all_index.saturating_sub(index) as f64 / all_index.max(1) as f64, mb(all_dict.saturating_sub(dict)));
        }
    }
}

/// The heaviest files of the same copy, without any rebuild: per path, the documents (Scope), the text,
/// the largest document, and the exact duplicates of content it holds (the same content in several
/// documents, a scope that receives the lines it covers).
///
///     LUCIVY_FAMILY_INDEX=<copy> LUCIVY_FAMILY_MAP=<tsv> [LUCIVY_FAMILY_TOP=25] \
///     cargo test --release -p lucivy-core --test test_weight_by_family heaviest_files -- --ignored --nocapture
#[test]
#[ignore]
fn heaviest_files() {
    use std::hash::{Hash, Hasher};
    let source_path = std::env::var("LUCIVY_FAMILY_INDEX").expect("LUCIVY_FAMILY_INDEX");
    let top: usize = std::env::var("LUCIVY_FAMILY_TOP").map(|v| v.parse().unwrap()).unwrap_or(25);
    let paths: HashMap<u64, String> = std::fs::read_to_string(std::env::var("LUCIVY_FAMILY_MAP").expect("LUCIVY_FAMILY_MAP"))
        .unwrap().lines()
        .filter_map(|l| { let mut c = l.split('\t'); Some((c.next()?.parse().ok()?, c.next()?.to_string())) })
        .collect();
    let source = ShardedHandle::open(&source_path).unwrap();
    let docs = stored_documents(&source);
    let text: u64 = docs.iter().map(|d| d.text_bytes).sum();
    // path → (documents, text, largest, duplicate bytes)
    let mut by: HashMap<String, (u64, u64, u64, u64)> = HashMap::new();
    let mut seen: HashMap<(String, u64), ()> = HashMap::new();
    for d in &docs {
        let path = paths.get(&d.node).cloned().unwrap_or_else(|| "(sans Scope)".into());
        let content = d.fields.get("content").and_then(|v| v.as_str()).unwrap_or("");
        let mut h = std::collections::hash_map::DefaultHasher::new();
        content.hash(&mut h);
        let duplicate = seen.insert((path.clone(), h.finish()), ()).is_some();
        let e = by.entry(path).or_default();
        e.0 += 1;
        e.1 += d.text_bytes;
        e.2 = e.2.max(d.content_bytes);
        if duplicate { e.3 += d.content_bytes }
    }
    let mut rows: Vec<(String, (u64, u64, u64, u64))> = by.into_iter().collect();
    rows.sort_by(|a, b| b.1 .1.cmp(&a.1 .1));
    let mut running = 0u64;
    println!("== the {top} heaviest files: path | documents | text | cumulative share | largest document | duplicates within the file");
    for (path, (n, bytes, largest, dup)) in rows.iter().take(top) {
        running += bytes;
        println!("{path}\t{n}\t{}\t{:.1} %\t{}\t{}", mb(*bytes), 100.0 * running as f64 / text.max(1) as f64, mb(*largest), mb(*dup));
    }
    let dup_total: u64 = rows.iter().map(|r| r.1 .3).sum();
    println!("{} files ; duplicates of content within a file: {} in all", rows.len(), mb(dup_total));
}
