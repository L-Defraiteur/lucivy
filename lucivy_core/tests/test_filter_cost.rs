//! What a filtered search costs on a real index, and where: the same queries
//! without a filter and with `allowed_ids`, and the parts of the filter that a
//! view given once would not pay again (the set itself, its split by shard
//! through the router, the per-segment bitsets over the `_node_id` column).
//!
//!     LUCIVY_FILTER_INDEX=<COPY of an index: opening may write> \
//!     LUCIVY_FILTER_QUERIES=<json file: [{"label": …, "query": QueryConfig}]> \
//!     [LUCIVY_FILTER_COUNT=72652] [LUCIVY_FILTER_IDS=<file, one node id per line>] \
//!     cargo test --release -p lucivy-core --test test_filter_cost -- --ignored --nocapture

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use lucivy_core::handle::NODE_ID_FIELD;
use lucivy_core::query::QueryConfig;
use lucivy_core::sharded_handle::ShardedHandle;

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

/// Every live node id of the index.
fn live_ids(index: &ShardedHandle) -> Vec<u64> {
    let mut ids = Vec::new();
    for shard in 0..index.num_shards() {
        let handle = index.shard(shard).unwrap();
        for seg in handle.reader.searcher().segment_readers() {
            let column = seg.fast_fields().u64(NODE_ID_FIELD).unwrap();
            let alive = seg.alive_bitset();
            for doc in 0..seg.max_doc() {
                if alive.is_some_and(|a| !a.is_alive(doc)) { continue }
                if let Some(id) = column.first(doc) { ids.push(id) }
            }
        }
    }
    ids
}

/// The two filter parts paid at every query today: the split by shard, then
/// a bitset per segment over its `_node_id` column (same code as
/// `build_node_filter_bitset`). Returns (split ms, bitsets ms, segments).
fn filter_parts(index: &ShardedHandle, set: &HashSet<u64>) -> (f64, f64, usize) {
    let t = Instant::now();
    let mut groups: Vec<HashSet<u64>> = vec![HashSet::new(); index.num_shards()];
    for &id in set {
        if let Some(sid) = index.shard_for_node_id(id) { groups[sid].insert(id); }
    }
    let split = ms(t);
    let t = Instant::now();
    let mut segments = 0;
    for (shard, group) in groups.iter().enumerate() {
        if group.is_empty() { continue }
        let handle = index.shard(shard).unwrap();
        for seg in handle.reader.searcher().segment_readers() {
            segments += 1;
            let column = seg.fast_fields().u64(NODE_ID_FIELD).unwrap();
            let mut bitset = ld_lucivy::BitSet::with_max_value(seg.max_doc());
            for doc in 0..seg.max_doc() {
                if group.contains(&column.first(doc).unwrap_or(u64::MAX)) { bitset.insert(doc) }
            }
            std::hint::black_box(ld_lucivy::fastfield::AliveBitSet::from_bitset(&bitset));
        }
    }
    (split, ms(t), segments)
}

#[test]
#[ignore]
fn filter_cost_of_an_index() {
    let path = std::env::var("LUCIVY_FILTER_INDEX").expect("LUCIVY_FILTER_INDEX");
    let queries: Vec<serde_json::Value> = serde_json::from_str(
        &std::fs::read_to_string(std::env::var("LUCIVY_FILTER_QUERIES").expect("LUCIVY_FILTER_QUERIES")).unwrap()).unwrap();
    let t = Instant::now();
    let index = ShardedHandle::open(&path).unwrap();
    index.wait_dictionary_folds();
    println!("opened in {:.0} ms", ms(t));
    let all = live_ids(&index);
    let chosen: Vec<u64> = match std::env::var("LUCIVY_FILTER_IDS") {
        Ok(file) => std::fs::read_to_string(file).unwrap().lines()
            .filter_map(|l| l.trim().parse().ok()).collect(),
        Err(_) => {
            // Deterministic spread over the live ids: every k-th of a shuffled order.
            let count: usize = std::env::var("LUCIVY_FILTER_COUNT").map(|v| v.parse().unwrap()).unwrap_or(72_652);
            let mut ids = all.clone();
            let mut x: u64 = 0x9E3779B97F4A7C15;
            for i in (1..ids.len()).rev() {
                x ^= x << 13; x ^= x >> 7; x ^= x << 17;
                ids.swap(i, (x % (i as u64 + 1)) as usize);
            }
            ids.truncate(count.min(ids.len()));
            ids
        }
    };
    let t = Instant::now();
    let set: HashSet<u64> = chosen.iter().copied().collect();
    let build_set = ms(t);
    let (split, bitsets, segments) = filter_parts(&index, &set);
    let (split2, bitsets2, _) = filter_parts(&index, &set);
    println!("{} live documents, {} allowed ; set built in {build_set:.1} ms ; split by shard {:.1} ms ; \
        bitsets over {segments} segments {:.1} ms (second run {:.1} / {:.1})",
        all.len(), set.len(), split, bitsets, split2, bitsets2);
    println!("query\tnone ms\tfiltered ms\tdelta ms\thits none\thits filtered");
    for entry in &queries {
        let label = entry["label"].as_str().unwrap();
        let query: QueryConfig = serde_json::from_value(entry["query"].clone()).unwrap();
        let mut none = Vec::new();
        let mut filtered = Vec::new();
        let mut hits = (0, 0);
        for _ in 0..5 {
            let sink = Arc::new(ld_lucivy::query::HighlightSink::new());
            let t = Instant::now();
            hits.0 = index.search(&query, 20, Some(sink)).unwrap().len();
            none.push(ms(t));
            let sink = Arc::new(ld_lucivy::query::HighlightSink::new());
            let copy = set.clone();
            let t = Instant::now();
            hits.1 = index.search_filtered(&query, 20, Some(sink), copy).unwrap().len();
            filtered.push(ms(t));
        }
        let (n, f) = (median(none), median(filtered));
        println!("{label}\t{n:.1}\t{f:.1}\t{:.1}\t{}\t{}", f - n, hits.0, hits.1);
    }
}
