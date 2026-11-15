//! Two indexes that should hold the same documents (a fresh index and a rebuilt one): are their stored
//! documents the same, are their BM25 statistics the same, and do the same queries rank the same
//! documents? Documents are named by the hash of their stored `content` and `name`, since `_node_id`
//! differs between two bases. Read-only on copies.
//!
//! With `LUCIVY_CMP_B_VISIBLE=1`, B is searched filtered to its documents whose content A also holds:
//! the visible versions of an index that keeps closed ones, as rag3weaver's visibility filter would.
//!
//!     LUCIVY_CMP_A=<copy> LUCIVY_CMP_B=<copy> LUCIVY_CMP_QUERIES=<json [{"label", "query"}]> [LUCIVY_CMP_B_VISIBLE=1] \
//!     cargo test --release -p lucivy-core --test test_compare_two_indexes -- --ignored --nocapture

use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};

use ld_lucivy::schema::OwnedValue;
use ld_lucivy::Document;
use lucivy_core::handle::NODE_ID_FIELD;
use lucivy_core::query::QueryConfig;
use lucivy_core::sharded_handle::ShardedHandle;

fn identity(content: &str, name: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    content.hash(&mut h);
    name.hash(&mut h);
    h.finish()
}

/// `_node_id` → identity, and the multiset of identities.
fn documents(index: &ShardedHandle) -> (HashMap<u64, u64>, BTreeMap<u64, u32>) {
    let (mut by_node, mut multiset) = (HashMap::new(), BTreeMap::new());
    for shard in 0..index.num_shards() {
        let handle = index.shard(shard).unwrap();
        let (node_f, content_f, name_f) = (handle.field(NODE_ID_FIELD).unwrap(), handle.field("content"), handle.field("name"));
        let searcher = handle.reader.searcher();
        for seg in searcher.segment_readers() {
            let store = seg.get_store_reader(64).unwrap();
            for doc_id in 0..seg.max_doc() {
                if seg.alive_bitset().is_some_and(|a| !a.is_alive(doc_id)) { continue }
                let doc: ld_lucivy::LucivyDocument = store.get(doc_id).unwrap();
                let (mut node, mut content, mut name) = (0u64, String::new(), String::new());
                for (f, v) in doc.iter_fields_and_values() {
                    match v.into() {
                        OwnedValue::U64(n) if f == node_f => node = n,
                        OwnedValue::Str(s) if Some(f) == content_f => content = s,
                        OwnedValue::Str(s) if Some(f) == name_f => name = s,
                        _ => {}
                    }
                }
                let id = identity(&content, &name);
                by_node.insert(node, id);
                *multiset.entry(id).or_insert(0) += 1;
            }
        }
    }
    (by_node, multiset)
}

fn stats(index: &ShardedHandle) -> String {
    let handle = index.shard(0).unwrap();
    let mut out = format!("N {}", index.num_docs());
    for name in ["content", "docstring", "name"] {
        let Some(f) = handle.field(name) else { continue };
        let tokens: u64 = (0..index.num_shards()).map(|s| index.shard(s).unwrap().reader.searcher().total_num_tokens(f).unwrap()).sum();
        out.push_str(&format!(", tokens {name} {tokens}"));
    }
    let segments: usize = (0..index.num_shards()).map(|s| index.shard(s).unwrap().reader.searcher().segment_readers().len()).sum();
    format!("{out}, {segments} segments")
}

#[test]
#[ignore]
fn compare_two_indexes() {
    let a = ShardedHandle::open(&std::env::var("LUCIVY_CMP_A").expect("LUCIVY_CMP_A")).unwrap();
    let b = ShardedHandle::open(&std::env::var("LUCIVY_CMP_B").expect("LUCIVY_CMP_B")).unwrap();
    let queries: Vec<serde_json::Value> = serde_json::from_str(&std::fs::read_to_string(std::env::var("LUCIVY_CMP_QUERIES").unwrap()).unwrap()).unwrap();
    println!("A: {}\nB: {}", stats(&a), stats(&b));
    let (a_docs, a_set) = documents(&a);
    let (b_docs, b_set) = documents(&b);
    let only_a: u32 = a_set.iter().map(|(k, n)| n.saturating_sub(*b_set.get(k).unwrap_or(&0))).sum();
    let only_b: u32 = b_set.iter().map(|(k, n)| n.saturating_sub(*a_set.get(k).unwrap_or(&0))).sum();
    println!("stored documents: A {} B {} ; only in A {only_a}, only in B {only_b}", a_docs.len(), b_docs.len());
    let visible: Option<std::collections::HashSet<u64>> = std::env::var("LUCIVY_CMP_B_VISIBLE").is_ok_and(|v| v == "1")
        .then(|| b_docs.iter().filter(|(_, id)| a_set.contains_key(id)).map(|(n, _)| *n).collect());
    if let Some(v) = &visible { println!("B searched filtered to {} documents whose content A holds", v.len()) }
    for entry in &queries {
        let label = entry["label"].as_str().unwrap();
        let q: QueryConfig = serde_json::from_value(entry["query"].clone()).unwrap();
        let top = |index: &ShardedHandle, docs: &HashMap<u64, u64>, filter: Option<&std::collections::HashSet<u64>>| -> Vec<(u64, f32)> {
            let r = match filter { Some(f) => index.search_filtered(&q, 10, None, f.clone()).unwrap(), None => index.search(&q, 10, None).unwrap() };
            index.node_ids_of(&r).unwrap().into_iter().zip(r.iter().map(|x| x.score)).map(|(n, s)| (*docs.get(&n).unwrap_or(&0), s)).collect()
        };
        let (ta, tb) = (top(&a, &a_docs, None), top(&b, &b_docs, visible.as_ref()));
        let same_ids = ta.iter().map(|x| x.0).collect::<Vec<_>>() == tb.iter().map(|x| x.0).collect::<Vec<_>>();
        let worst = ta.iter().zip(&tb).map(|(x, y)| ((x.1 - y.1) as f64).abs() / (x.1 as f64).abs().max(1e-9)).fold(0f64, f64::max);
        let first = ta.iter().zip(&tb).position(|(x, y)| x.0 != y.0);
        println!("{label}\t{}\tA {} hits B {} hits\tfirst divergence {:?}\tscore gap {worst:.2e}\tA top3 {:?}\tB top3 {:?}",
            if same_ids { "SAME" } else { "DIFF" }, ta.len(), tb.len(), first,
            ta.iter().take(3).map(|x| format!("{:x}:{:.3}", x.0 >> 40, x.1)).collect::<Vec<_>>(),
            tb.iter().take(3).map(|x| format!("{:x}:{:.3}", x.0 >> 40, x.1)).collect::<Vec<_>>());
    }
}
