//! Diagnostic (ignored): what a sharded index holds, read from its stored
//! documents. LUCIVY_DIAG_INDEX = a COPY of a sharded index directory (opening
//! may write). For the live documents of each text field: count, total stored
//! bytes, distinct contents; and the distinct `_node_id`s.

use lucivy_core::sharded_handle::ShardedHandle;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};

#[test]
#[ignore]
fn diag_poids_du_texte() {
    let path = std::env::var("LUCIVY_DIAG_INDEX").expect("LUCIVY_DIAG_INDEX");
    let index = ShardedHandle::open(&path).unwrap();
    let mut bytes: HashMap<String, u64> = HashMap::new();
    let mut docs_with: HashMap<String, u64> = HashMap::new();
    let mut distinct: HashMap<String, HashSet<u64>> = HashMap::new();
    let mut node_ids: HashSet<u64> = HashSet::new();
    let mut live = 0u64;
    let mut largest: Vec<(u64, String)> = Vec::new();
    for shard_id in 0..index.num_shards() {
        let shard = index.shard(shard_id).unwrap();
        let node_field = shard.field(lucivy_core::handle::NODE_ID_FIELD).unwrap();
        let text_fields: Vec<(String, ld_lucivy::schema::Field)> = shard.field_map.iter()
            .filter(|(_, f)| matches!(shard.schema.get_field_entry(*f).field_type(), ld_lucivy::schema::FieldType::Str(_)))
            .cloned().collect();
        let searcher = shard.reader.searcher();
        for seg in searcher.segment_readers() {
            let store = seg.get_store_reader(64).unwrap();
            let alive = seg.alive_bitset();
            for doc_id in 0..seg.max_doc() {
                if alive.is_some_and(|a| !a.is_alive(doc_id)) { continue; }
                let doc: ld_lucivy::LucivyDocument = store.get(doc_id).unwrap();
                live += 1;
                if let Some(v) = doc.get_first(node_field) {
                    let o: ld_lucivy::schema::OwnedValue = v.into();
                    if let ld_lucivy::schema::OwnedValue::U64(n) = o { node_ids.insert(n); }
                }
                for (name, f) in &text_fields {
                    let text: String = doc.get_first(*f).map(|v| {
                        let o: ld_lucivy::schema::OwnedValue = v.into();
                        match o { ld_lucivy::schema::OwnedValue::Str(s) => s, _ => String::new() }
                    }).unwrap_or_default();
                    if text.is_empty() { continue; }
                    *bytes.entry(name.clone()).or_default() += text.len() as u64;
                    *docs_with.entry(name.clone()).or_default() += 1;
                    let mut h = std::collections::hash_map::DefaultHasher::new();
                    text.hash(&mut h);
                    distinct.entry(name.clone()).or_default().insert(h.finish());
                    if name == "content" {
                        largest.push((text.len() as u64, text.lines().next().unwrap_or("").chars().take(80).collect()));
                    }
                }
            }
        }
    }
    eprintln!("live documents {live}, distinct _node_id {}", node_ids.len());
    let mut names: Vec<_> = bytes.keys().cloned().collect();
    names.sort();
    for name in names {
        eprintln!("field {name}: {} docs, {:.1} MB stored text, {} distinct contents ({:.1} MB if each counted once)",
            docs_with[&name], bytes[&name] as f64 / 1e6, distinct[&name].len(),
            0.0);
    }
    largest.sort_by(|a, b| b.0.cmp(&a.0));
    let total: u64 = largest.iter().map(|l| l.0).sum();
    let top: u64 = largest.iter().take(largest.len() / 10).map(|l| l.0).sum();
    eprintln!("content: largest 10 % of documents hold {:.1} % of its bytes", 100.0 * top as f64 / total.max(1) as f64);
    let mut sizes: Vec<u64> = largest.iter().map(|l| l.0).collect();
    sizes.sort_unstable();
    let q = |p: f64| sizes.get(((sizes.len() as f64 - 1.0) * p) as usize).copied().unwrap_or(0);
    eprintln!("content size per doc: p50 {} B, p90 {} B, p99 {} B, max {} B", q(0.5), q(0.9), q(0.99), q(1.0));
    for (len, head) in largest.iter().take(8) {
        eprintln!("  {len:>9} B  {head}");
    }
}


/// One TSV line per live document (LUCIVY_DIAG_OUT): shard, _node_id, bytes of
/// content, name, longest line, whitespace share, markers, content hash, first line.
#[test]
#[ignore]
fn diag_documents_tsv() {
    use std::io::Write;
    let path = std::env::var("LUCIVY_DIAG_INDEX").expect("LUCIVY_DIAG_INDEX");
    let out_path = std::env::var("LUCIVY_DIAG_OUT").expect("LUCIVY_DIAG_OUT");
    let index = ShardedHandle::open(&path).unwrap();
    let mut out = std::io::BufWriter::new(std::fs::File::create(&out_path).unwrap());
    writeln!(out, "shard\tnode_id\tbytes\tname\tlongest_line\tspace_pct\tmarkers\thash\tfirst_line").unwrap();
    let text_of = |doc: &ld_lucivy::LucivyDocument, f: ld_lucivy::schema::Field| -> String {
        doc.get_first(f).map(|v| {
            let o: ld_lucivy::schema::OwnedValue = v.into();
            match o { ld_lucivy::schema::OwnedValue::Str(s) => s, _ => String::new() }
        }).unwrap_or_default()
    };
    let clean = |s: &str| s.replace(['\t', '\n', '\r'], " ");
    for shard_id in 0..index.num_shards() {
        let shard = index.shard(shard_id).unwrap();
        let node_field = shard.field(lucivy_core::handle::NODE_ID_FIELD).unwrap();
        let content = shard.field("content").unwrap();
        let name = shard.field("name").unwrap();
        let searcher = shard.reader.searcher();
        for seg in searcher.segment_readers() {
            let store = seg.get_store_reader(64).unwrap();
            let alive = seg.alive_bitset();
            for doc_id in 0..seg.max_doc() {
                if alive.is_some_and(|a| !a.is_alive(doc_id)) { continue; }
                let doc: ld_lucivy::LucivyDocument = store.get(doc_id).unwrap();
                let node = doc.get_first(node_field).and_then(|v| {
                    let o: ld_lucivy::schema::OwnedValue = v.into();
                    if let ld_lucivy::schema::OwnedValue::U64(n) = o { Some(n) } else { None }
                }).unwrap_or(u64::MAX);
                let text = text_of(&doc, content);
                let longest = text.lines().map(|l| l.len()).max().unwrap_or(0);
                let spaces = text.bytes().filter(|b| b.is_ascii_whitespace()).count();
                let space_pct = if text.is_empty() { 0 } else { 100 * spaces / text.len() };
                let head: String = text.chars().take(4000).collect::<String>().to_lowercase();
                let mut markers = Vec::new();
                for (m, label) in [("generated", "generated"), ("do not edit", "do-not-edit"), ("autogenerated", "generated"),
                                   ("copyright", "copyright"), ("license", "license"), ("sourcemappingurl", "sourcemap")] {
                    if head.contains(m) && !markers.contains(&label) { markers.push(label); }
                }
                let mut h = std::collections::hash_map::DefaultHasher::new();
                text.hash(&mut h);
                let first: String = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("").chars().take(100).collect();
                writeln!(out, "{shard_id}\t{node}\t{}\t{}\t{longest}\t{space_pct}\t{}\t{:016x}\t{}",
                    text.len(), clean(&text_of(&doc, name)).chars().take(80).collect::<String>(),
                    markers.join(","), h.finish(), clean(&first)).unwrap();
            }
        }
    }
    out.flush().unwrap();
}
