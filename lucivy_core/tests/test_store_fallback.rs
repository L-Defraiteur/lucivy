//! Quick test: reopen persisted index and run regex query to verify store fallback.

#[test]
fn test_regex_on_persisted_index() {
    let base = "/home/luciedefraiteur/lucivy_bench_sharding/round_robin";
    if !std::path::Path::new(base).exists() {
        eprintln!("Skipping — no persisted index at {base}");
        return;
    }

    // Remove lock files
    for i in 0..4 {
        let lock = format!("{base}/shard_{i}/.lucivy-writer.lock");
        let _ = std::fs::remove_file(&lock);
    }

    let handle = lucivy_core::sharded_handle::ShardedHandle::open(base)
        .expect("open persisted index");

    eprintln!("Opened {} shards, {} total docs", handle.num_shards(), handle.num_docs());

    // Test regex contains
    let queries = vec![
        ("regex 'pr_[a-z]+'", lucivy_core::query::QueryConfig {
            query_type: "contains".into(),
            field: Some("content".into()),
            value: Some("pr_[a-z]+".into()),
            regex: Some(true),
            ..Default::default()
        }),
        ("contains 'mutex'", lucivy_core::query::QueryConfig {
            query_type: "contains".into(),
            field: Some("content".into()),
            value: Some("mutex".into()),
            ..Default::default()
        }),
        ("fuzzy 'schdule' d=1", lucivy_core::query::QueryConfig {
            query_type: "contains".into(),
            field: Some("content".into()),
            value: Some("schdule".into()),
            distance: Some(1),
            ..Default::default()
        }),
    ];

    for (label, config) in &queries {
        let start = std::time::Instant::now();
        let results = handle.search(config, 20, None).unwrap();
        let ms = start.elapsed().as_millis();
        eprintln!("{label}: {} hits in {}ms", results.len(), ms);
    }
}
