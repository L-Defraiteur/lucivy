//! The light profile, measured on a real index: the stored documents of a
//! COPY are written again into one index per profile (positions or not, a
//! shard dictionary or a dictionary per segment), then each says its bytes by
//! family (`index_report`) and the time of a set of queries, whose answers
//! must be the same in every profile.
//!
//!     LUCIVY_PROFILE_SOURCE=<copy of an index> \
//!     LUCIVY_PROFILE_OUT=<empty directory on disk, not /tmp> \
//!     LUCIVY_PROFILE_QUERIES=<json file: [{"label": …, "query": QueryConfig}]> \
//!     [LUCIVY_PROFILE_VARIANTS=positions,no_positions,segment_dictionary] \
//!     [LUCIVY_PROFILE_COMMIT_EVERY=20000] \
//!     cargo test --release -p lucivy-core --test test_light_profile -- --ignored --nocapture

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Instant;

use ld_lucivy::schema::OwnedValue;
use ld_lucivy::Document;
use lucivy_core::handle::NODE_ID_FIELD;
use lucivy_core::query::{QueryConfig, SchemaConfig};
use lucivy_core::sharded_handle::ShardedHandle;

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} is required"))
}

fn variant_config(source: &SchemaConfig, variant: &str) -> SchemaConfig {
    let mut config = source.clone();
    match variant {
        "positions" => {
            config.positions = Some(true);
            config.shared_dictionary = Some(true);
        }
        "no_positions" => {
            config.positions = Some(false);
            config.shared_dictionary = Some(true);
            config.derived_in_ram = None;
        }
        "segment_dictionary" => {
            // A dictionary per segment is the v3 layout: v4 refuses `shared_dictionary: false`.
            config.positions = Some(true);
            config.shared_dictionary = Some(false);
            config.sfx_version = Some(3);
        }
        "no_positions_segment_dictionary" => {
            config.positions = Some(false);
            config.shared_dictionary = Some(false);
            config.sfx_version = Some(3);
            config.derived_in_ram = None;
        }
        other => panic!("unknown variant {other}"),
    }
    config
}

/// Every live stored document of `source`: (node_id, its stored fields as JSON).
fn stored_documents(source: &ShardedHandle) -> Vec<(u64, serde_json::Value)> {
    let mut out = Vec::new();
    for shard in 0..source.num_shards() {
        let handle = source.shard(shard).unwrap();
        let node_field = handle.field(NODE_ID_FIELD).unwrap();
        let fields: Vec<(String, ld_lucivy::schema::Field)> = handle.field_map.iter()
            .filter(|(name, _)| name != NODE_ID_FIELD)
            .cloned()
            .collect();
        let searcher = handle.reader.searcher();
        for segment in searcher.segment_readers() {
            let store = segment.get_store_reader(64).unwrap();
            let alive = segment.alive_bitset();
            for doc_id in 0..segment.max_doc() {
                if alive.is_some_and(|a| !a.is_alive(doc_id)) {
                    continue;
                }
                let doc: ld_lucivy::LucivyDocument = store.get(doc_id).unwrap();
                let mut node = None;
                let mut object = serde_json::Map::new();
                for (field, value) in doc.iter_fields_and_values() {
                    let owned: OwnedValue = value.into();
                    if field == node_field {
                        if let OwnedValue::U64(n) = owned { node = Some(n) }
                        continue;
                    }
                    let Some((name, _)) = fields.iter().find(|(_, f)| *f == field) else { continue };
                    let json = match owned {
                        OwnedValue::Str(s) => serde_json::Value::String(s),
                        OwnedValue::U64(n) => serde_json::json!(n),
                        OwnedValue::I64(n) => serde_json::json!(n),
                        OwnedValue::F64(n) => serde_json::json!(n),
                        _ => continue,
                    };
                    object.insert(name.clone(), json);
                }
                out.push((node.expect("_node_id stored"), serde_json::Value::Object(object)));
            }
        }
    }
    out
}

fn answers(index: &ShardedHandle, query: &QueryConfig) -> (Vec<(u64, f32)>, f64, f64, usize) {
    let mut times = Vec::new();
    let mut ids = Vec::new();
    let mut spans = 0;
    for run in 0..5 {
        let sink = Arc::new(ld_lucivy::query::HighlightSink::new());
        let started = Instant::now();
        let results = index.search(query, 20, Some(sink.clone())).unwrap();
        let found: Vec<(u64, f32)> = index.node_ids_of(&results).unwrap().into_iter()
            .zip(results.iter().map(|r| r.score)).collect();
        times.push(started.elapsed().as_secs_f64() * 1000.0);
        if run == 0 {
            ids = found;
            spans = sink.all_entries().len();
        }
    }
    let first = times[0];
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    (ids, first, times[2], spans)
}

#[test]
#[ignore]
fn light_profile_of_an_index() {
    let source_path = env("LUCIVY_PROFILE_SOURCE");
    let out = std::path::PathBuf::from(env("LUCIVY_PROFILE_OUT"));
    assert!(!out.to_string_lossy().starts_with("/tmp"), "LUCIVY_PROFILE_OUT: on disk, not /tmp");
    let queries: Vec<serde_json::Value> =
        serde_json::from_str(&std::fs::read_to_string(env("LUCIVY_PROFILE_QUERIES")).unwrap()).unwrap();
    let variants = std::env::var("LUCIVY_PROFILE_VARIANTS")
        .unwrap_or_else(|_| "positions,no_positions,segment_dictionary".into());
    let commit_every: usize = std::env::var("LUCIVY_PROFILE_COMMIT_EVERY")
        .map(|v| v.parse().unwrap()).unwrap_or(20_000);

    let source_config: SchemaConfig = serde_json::from_str(
        &std::fs::read_to_string(format!("{source_path}/_shard_config.json")).unwrap()).unwrap();
    let source = ShardedHandle::open(&source_path).unwrap();
    let started = Instant::now();
    let documents = stored_documents(&source);
    println!("source : {} documents read in {:.1} s", documents.len(), started.elapsed().as_secs_f64());
    println!("source :\n{}", source.index_report().unwrap().summary());
    drop(source);

    let mut reference: Option<Vec<(String, Vec<(u64, f32)>)>> = None;
    for variant in variants.split(',') {
        let path = out.join(variant);
        assert!(!path.exists(), "{} exists: the measure writes into an empty directory", path.display());
        let config = variant_config(&source_config, variant);
        let index = ShardedHandle::create(path.to_str().unwrap(), &config).unwrap();
        let started = Instant::now();
        for (n, (node, fields)) in documents.iter().enumerate() {
            index.add_document_json(*node, fields).unwrap();
            if (n + 1) % commit_every == 0 {
                index.commit().unwrap();
            }
        }
        index.commit().unwrap();
        index.wait_merges_quiet().unwrap();
        index.wait_dictionary_folds();
        let indexing = started.elapsed().as_secs_f64();
        let report = index.index_report().unwrap();
        println!("\n== {variant} : indexed in {indexing:.1} s, {} on disk", report.index_bytes);
        println!("{}", report.summary());
        let mut seen = Vec::new();
        println!("query\tfirst ms\tmedian ms\thits\tspans");
        for entry in &queries {
            let label = entry["label"].as_str().unwrap().to_string();
            let query: QueryConfig = serde_json::from_value(entry["query"].clone()).unwrap();
            let (ids, first, median, spans) = answers(&index, &query);
            println!("{label}\t{first:.1}\t{median:.1}\t{}\t{spans}", ids.len());
            seen.push((label, ids));
        }
        match &reference {
            None => reference = Some(seen),
            Some(expected) => {
                for ((label, got), (_, want)) in seen.iter().zip(expected) {
                    let g: BTreeSet<u64> = got.iter().map(|x| x.0).collect();
                    let w: BTreeSet<u64> = want.iter().map(|x| x.0).collect();
                    if g != w {
                        // Ties at the cut of the top-k are not a different answer: say the scores.
                        let last = |v: &Vec<(u64, f32)>| v.last().map(|x| x.1).unwrap_or(0.0);
                        let only = |a: &Vec<(u64, f32)>, b: &BTreeSet<u64>| a.iter()
                            .filter(|x| !b.contains(&x.0)).map(|x| format!("{}:{}", x.0, x.1)).collect::<Vec<_>>().join(" ");
                        println!("DIFFERENT ANSWERS for {label} in {variant}: {} in common of {} ; last score {} vs {} ; only here [{}] ; only in the first [{}]",
                            g.intersection(&w).count(), w.len(), last(got), last(want), only(got, &w), only(want, &g));
                    }
                    let scores_differ = got.iter().zip(want).any(|(a, b)| a.0 == b.0 && a.1.to_bits() != b.1.to_bits());
                    if scores_differ { println!("SCORES DIFFER for {label} in {variant}"); }
                }
            }
        }
        index.close().unwrap();
    }
}
