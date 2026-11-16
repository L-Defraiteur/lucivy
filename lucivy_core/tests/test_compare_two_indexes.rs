//! Two indexes that should hold the same documents (a fresh index and a rebuilt one): are their stored
//! documents the same, are their BM25 statistics the same, and do the same queries rank the same
//! documents? Documents are named by the hash of their stored `content` and `name`, since `_node_id`
//! differs between two bases. Read-only on copies.
//!
//! With `LUCIVY_CMP_B_VISIBLE=1`, B is searched filtered to its documents whose content A also holds:
//! the visible versions of an index that keeps closed ones, as rag3weaver's visibility filter would.
//!
//! With `LUCIVY_CMP_DETAIL=1`, each query whose top 10 differs prints the top 20 of both sides, each document
//! marked `both` or `only-A` / `only-B` (its content is nowhere the other side searches): a rank change from a
//! missing document is told from a scoring one, and a permutation between equal scores from both. With
//! `LUCIVY_CMP_ONLY_OUT=<prefix>`, the `_node_id` of those one-sided documents go to `<prefix>-only-A.txt` and
//! `<prefix>-only-B.txt`, to be looked up in each base.
//!
//!     LUCIVY_CMP_A=<copy> LUCIVY_CMP_B=<copy> LUCIVY_CMP_QUERIES=<json [{"label", "query"}]> \
//!     [LUCIVY_CMP_B_IDS=<file> | LUCIVY_CMP_B_VISIBLE=1] [LUCIVY_CMP_DETAIL=1] [LUCIVY_CMP_ONLY_OUT=<prefix>] \
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
        let tokens: u64 = (0..index.num_shards())
            .flat_map(|s| index.shard(s).unwrap().reader.searcher().segment_readers().to_vec())
            .map(|seg| seg.inverted_index(f).map(|i| i.total_num_tokens()).unwrap_or(0))
            .sum();
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
    // `LUCIVY_CMP_B_IDS`: B filtered by a file of `_node_id` (one per line), the set its caller passes;
    // `LUCIVY_CMP_B_VISIBLE=1`: an approximation, B's documents whose content A also holds.
    let visible: Option<std::collections::HashSet<u64>> = match std::env::var("LUCIVY_CMP_B_IDS") {
        Ok(file) => Some(std::fs::read_to_string(file).unwrap().lines().filter_map(|l| l.trim().parse().ok()).collect()),
        Err(_) => std::env::var("LUCIVY_CMP_B_VISIBLE").is_ok_and(|v| v == "1")
            .then(|| b_docs.iter().filter(|(_, id)| a_set.contains_key(id)).map(|(n, _)| *n).collect()),
    };
    if let Some(v) = &visible { println!("B searched filtered to {} documents whose content A holds", v.len()) }
    // `LUCIVY_CMP_DETAIL=1`: for each query whose top 10 differs, the top 20 of both sides, each document
    // marked by whether the other side can return it at all (`both`) or holds its content nowhere it is
    // searched (`only-A` / `only-B`), so a rank change from a missing document is told from a scoring one.
    let detail = std::env::var("LUCIVY_CMP_DETAIL").is_ok_and(|v| v == "1");
    let b_searched: std::collections::HashSet<u64> = match &visible {
        Some(v) => b_docs.iter().filter(|(n, _)| v.contains(n)).map(|(_, id)| *id).collect(),
        None => b_docs.values().copied().collect(),
    };
    let a_searched: std::collections::HashSet<u64> = a_docs.values().copied().collect();
    if detail {
        let only_a = a_searched.iter().filter(|id| !b_searched.contains(id)).count();
        let only_b = b_searched.iter().filter(|id| !a_searched.contains(id)).count();
        println!("searched contents: A {} distinct, B {} distinct ; only in A {only_a}, only in B {only_b}", a_searched.len(), b_searched.len());
        // `LUCIVY_CMP_ONLY_OUT=<prefix>`: the `_node_id` of the searched documents whose content the other side
        // never searches, one per line, in `<prefix>-only-A.txt` (ids of A) and `<prefix>-only-B.txt` (ids of B).
        if let Ok(prefix) = std::env::var("LUCIVY_CMP_ONLY_OUT") {
            let mut oa: Vec<u64> = a_docs.iter().filter(|(_, id)| !b_searched.contains(id)).map(|(n, _)| *n).collect();
            let mut ob: Vec<u64> = b_docs.iter().filter(|(n, id)| visible.as_ref().is_none_or(|v| v.contains(n)) && !a_searched.contains(id)).map(|(n, _)| *n).collect();
            oa.sort_unstable(); ob.sort_unstable();
            std::fs::write(format!("{prefix}-only-A.txt"), oa.iter().map(|n| format!("{n}\n")).collect::<String>()).unwrap();
            std::fs::write(format!("{prefix}-only-B.txt"), ob.iter().map(|n| format!("{n}\n")).collect::<String>()).unwrap();
            println!("only-side ids written: {} of A, {} of B", oa.len(), ob.len());
        }
    }
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
        if detail && !same_ids {
            let run = |index: &ShardedHandle, docs: &HashMap<u64, u64>, filter: Option<&std::collections::HashSet<u64>>| -> Vec<(u64, f32)> {
                let r = match filter { Some(f) => index.search_filtered(&q, 20, None, f.clone()).unwrap(), None => index.search(&q, 20, None).unwrap() };
                index.node_ids_of(&r).unwrap().into_iter().zip(r.iter().map(|x| x.score)).map(|(n, s)| (*docs.get(&n).unwrap_or(&0), s)).collect()
            };
            let (da, db) = (run(&a, &a_docs, None), run(&b, &b_docs, visible.as_ref()));
            let mark = |id: u64, other: &std::collections::HashSet<u64>, side: &str| if other.contains(&id) { "both".to_string() } else { format!("only-{side}") };
            for i in 0..da.len().max(db.len()) {
                let ca = da.get(i).map(|x| format!("{:x}:{:.4}:{}", x.0 >> 40, x.1, mark(x.0, &b_searched, "A"))).unwrap_or_default();
                let cb = db.get(i).map(|x| format!("{:x}:{:.4}:{}", x.0 >> 40, x.1, mark(x.0, &a_searched, "B"))).unwrap_or_default();
                println!("  {label}\t#{}\tA {ca}\tB {cb}", i + 1);
            }
        }
    }
}
