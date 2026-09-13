//! Where the time goes when the documents of a hit list are read back from
//! the document store — measured on an existing index, never rebuilt.
//!
//! Run (release, ignored):
//!   V3_INDEX_DIR=~/lucivy_bench/compare-4.1/dict-nopos BENCH_QUERY=mutex_lock \
//!   cargo test --release -p lucivy-core --test bench_docstore_fetch -- --ignored --nocapture
//!
//! Phases, each timed on the same hit list:
//!   search      the engine only (no fetch), as the harness reports it
//!   fastfield   `_node_id` from the fast field, no store access at all
//!   bytes       one `get_document_bytes` per hit, score order (seek + block decompression)
//!   doc         one `searcher.doc` per hit, score order (bytes + deserialization) — what
//!               every binding does today, sequentially
//!   doc sorted  the same, hits sorted by (segment, doc) first (block locality)
//!   doc par     the same, one thread per segment group (the ceiling for a parallel fetch)
//!   verify-like sorted doc ids inside each segment with an LRU of 1 / 4 / 100 blocks —
//!               the shape of `briques::stored::verify_stored`
use std::sync::Arc;
use std::time::Instant;

use ld_lucivy::schema::document::Value;
use ld_lucivy::{DocAddress, LucivyDocument};
use lucivy_core::handle::{LucivyHandle, NODE_ID_FIELD};
use lucivy_core::query::{self, QueryConfig};

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1e3
}

fn run(handle: &LucivyHandle, value: &str, strict: bool) -> Vec<(f32, DocAddress)> {
    let sink = Arc::new(ld_lucivy::query::HighlightSink::new());
    let config = QueryConfig {
        query_type: "contains".into(),
        field: Some("content".into()),
        value: Some(value.into()),
        strict_separators: Some(strict),
        ..Default::default()
    };
    let q = query::build_query(&config, &handle.schema, &handle.index, Some(sink)).unwrap();
    let searcher = handle.reader.searcher();
    let n = searcher.num_docs() as usize;
    let collector = ld_lucivy::collector::TopDocs::with_limit(n.max(1)).order_by_score();
    searcher.search(&*q, &collector).unwrap()
}

#[test]
#[ignore]
fn bench_docstore_fetch() {
    let dir = std::env::var("V3_INDEX_DIR").expect("V3_INDEX_DIR=<index dir>");
    let value = std::env::var("BENCH_QUERY").unwrap_or_else(|_| "mutex_lock".into());
    let strict = std::env::var("BENCH_RELAX").is_err();
    let rounds: usize = std::env::var("BENCH_ROUNDS").ok().and_then(|v| v.parse().ok()).unwrap_or(3);

    let t = Instant::now();
    let mmap = ld_lucivy::directory::MmapDirectory::open(&dir).unwrap();
    let handle = LucivyHandle::open(mmap).unwrap();
    let searcher = handle.reader.searcher();
    eprintln!("open {dir}: {:.1} ms, {} segments, {} docs", ms(t), searcher.segment_readers().len(), searcher.num_docs());
    let content = handle.field("content").unwrap();
    let nid = handle.field(NODE_ID_FIELD).unwrap();

    for round in 0..rounds {
        eprintln!("--- round {round} ({value:?}, strict={strict})");
        let t = Instant::now();
        let hits = run(&handle, &value, strict);
        eprintln!("search      {:>8.1} ms  {} hits", ms(t), hits.len());
        let searcher = handle.reader.searcher();

        // fast field only
        let t = Instant::now();
        let mut sum = 0u64;
        for (_, a) in &hits {
            let col = searcher.segment_reader(a.segment_ord).fast_fields().u64(NODE_ID_FIELD).unwrap();
            sum = sum.wrapping_add(col.first(a.doc_id).unwrap_or(0));
        }
        eprintln!("fastfield   {:>8.1} ms  (sum {sum})", ms(t));

        // bytes only, score order, one store reader per segment (100 blocks)
        let stores: Vec<_> = searcher.segment_readers().iter().map(|s| s.get_store_reader(100).unwrap()).collect();
        let t = Instant::now();
        let mut total = 0usize;
        for (_, a) in &hits {
            total += stores[a.segment_ord as usize].get_document_bytes(a.doc_id).unwrap().len();
        }
        eprintln!("bytes       {:>8.1} ms  {:.1} MB of serialized documents", ms(t), total as f64 / 1e6);

        // full doc, score order — what the bindings do
        let t = Instant::now();
        let mut text = 0usize;
        for (_, a) in &hits {
            let d: LucivyDocument = searcher.doc(*a).unwrap();
            text += d.get_first(content).and_then(|v| v.as_value().as_str()).map_or(0, |s| s.len());
            let _ = d.get_first(nid);
        }
        let st = searcher.doc_store_cache_stats();
        eprintln!("doc         {:>8.1} ms  {:.1} MB of content; cache hits {} misses {}", ms(t), text as f64 / 1e6, st.cache_hits, st.cache_misses);

        // full doc, sorted by (segment, doc)
        let mut sorted = hits.clone();
        sorted.sort_by_key(|(_, a)| (a.segment_ord, a.doc_id));
        let t = Instant::now();
        let mut text2 = 0usize;
        for (_, a) in &sorted {
            let d: LucivyDocument = stores[a.segment_ord as usize].get(a.doc_id).unwrap();
            text2 += d.get_first(content).and_then(|v| v.as_value().as_str()).map_or(0, |s| s.len());
        }
        assert_eq!(text, text2);
        eprintln!("doc sorted  {:>8.1} ms", ms(t));

        // parallel by segment group
        let nseg = searcher.segment_readers().len();
        let mut by_seg: Vec<Vec<u32>> = vec![Vec::new(); nseg];
        for (_, a) in &hits {
            by_seg[a.segment_ord as usize].push(a.doc_id);
        }
        let threads: usize = std::env::var("BENCH_THREADS").ok().and_then(|v| v.parse().ok()).unwrap_or(8);
        let t = Instant::now();
        let text3: usize = std::thread::scope(|s| {
            let jobs: Vec<_> = (0..threads).map(|k| {
                let searcher = &searcher;
                let by_seg = &by_seg;
                s.spawn(move || {
                    let mut n = 0usize;
                    for seg in (k..nseg).step_by(threads) {
                        if by_seg[seg].is_empty() { continue; }
                        let store = searcher.segment_reader(seg as u32).get_store_reader(100).unwrap();
                        for &doc in &by_seg[seg] {
                            let d: LucivyDocument = store.get(doc).unwrap();
                            n += d.get_first(content).and_then(|v| v.as_value().as_str()).map_or(0, |s| s.len());
                        }
                    }
                    n
                })
            }).collect();
            jobs.into_iter().map(|j| j.join().unwrap()).sum()
        });
        assert_eq!(text, text3);
        eprintln!("doc par{:<3}  {:>8.1} ms", threads, ms(t));

        // verify-like: sorted doc ids per segment, small LRU
        for cache in [1usize, 4, 100] {
            let t = Instant::now();
            let mut n = 0usize;
            for seg in 0..nseg {
                if by_seg[seg].is_empty() { continue; }
                let store = searcher.segment_reader(seg as u32).get_store_reader(cache).unwrap();
                let mut ids = by_seg[seg].clone();
                ids.sort_unstable();
                for doc in ids {
                    let d: LucivyDocument = store.get(doc).unwrap();
                    n += d.get_first(content).and_then(|v| v.as_value().as_str()).map_or(0, |s| s.len());
                }
            }
            assert_eq!(text, n);
            eprintln!("verify c{:<3}  {:>8.1} ms", cache, ms(t));
        }
    }
}
