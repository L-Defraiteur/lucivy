//! LUCE snapshot round-trip on a **v3 sharded** index.
//!
//! The existing `test_luce_roundtrip` reads `playground/dataset.luce`, which
//! predates the v3 default: nothing exercised export → import on an index
//! carrying `.sibling_v3`, `.word_sfxpost`, `.word_pos_map`, `.bytemap`,
//! `.termtexts`, `.posmap`. This builds one, snapshots it, imports it into a
//! fresh directory and requires every query of the panel to answer
//! identically — count, scores to the bit, span counts and top ids.
//!
//!   cargo test --release -p lucivy-core --test test_luce_v3_roundtrip -- --nocapture
//!
//! With a real corpus (same knobs as the parity test), on a bigger index:
//!
//!   LUCE_ROOT=/tmp/linux-bench LUCE_LIST=/tmp/corpus_indexed.list LUCE_MAX_DOCS=3000 \
//!   cargo test --release -p lucivy-core --test test_luce_v3_roundtrip -- --nocapture

use lucivy_core::query::{QueryConfig, SchemaConfig};
use lucivy_core::sharded_handle::ShardedHandle;
use lucivy_core::snapshot;
use std::path::Path;

/// (count, top-10 (node_id, score, span count)) for one query.
type Answer = (usize, Vec<(u64, f32, usize)>);

fn run_panel(h: &ShardedHandle, panel: &[(&str, QueryConfig)]) -> Vec<(String, Answer)> {
    let nid = h.field("_node_id").unwrap();
    panel
        .iter()
        .map(|(name, q)| {
            let hits = h.search_with_docs(q, 10_000).unwrap_or_else(|e| panic!("{name}: {e}"));
            let top = hits
                .iter()
                .take(10)
                .map(|hit| {
                    let id = {
                        use ld_lucivy::schema::Value;
                        hit.doc.get_first(nid).and_then(|v| v.as_u64()).unwrap_or(u64::MAX)
                    };
                    let spans: usize = hit.highlights.values().map(|v| v.len()).sum();
                    (id, hit.score, spans)
                })
                .collect();
            (name.to_string(), (hits.len(), top))
        })
        .collect()
}

/// SFX format version of every segment of every shard.
fn sfx_versions(h: &ShardedHandle) -> Vec<u8> {
    (0..h.num_shards())
        .filter_map(|i| h.shard(i))
        .flat_map(|s| s.sfx_versions())
        .flatten()
        .collect()
}

fn contains(field: &str, value: &str) -> QueryConfig {
    QueryConfig {
        query_type: "contains".into(),
        field: Some(field.into()),
        value: Some(value.into()),
        ..Default::default()
    }
}

#[test]
fn luce_v3_sharded_roundtrip() {
    luce_sharded_roundtrip(3);
}

/// A shard dictionary (`sfx_version` 4) travels in a snapshot too: its
/// generations (`dict-<g>.*`) are bundled with the shard, the imported
/// index is still a dictionary index and answers the same.
#[test]
fn luce_dictionary_sharded_roundtrip() {
    luce_sharded_roundtrip(4);
}

fn luce_sharded_roundtrip(sfx_version: u8) {
    let config: SchemaConfig = serde_json::from_value(serde_json::json!({
        "fields": [
            {"name": "path", "type": "text"},
            {"name": "content", "type": "text"},
            {"name": "extension", "type": "text"}
        ],
        "sfx_version": sfx_version,
        "shards": 4
    }))
    .unwrap();
    let settings_of = |h: &ShardedHandle| -> Vec<u8> {
        (0..h.num_shards()).map(|i| h.shard(i).unwrap().index.settings().sfx_version).collect()
    };

    let dir = std::env::temp_dir().join(format!("lucivy_luce_v{sfx_version}_src"));
    let _ = std::fs::remove_dir_all(&dir);
    let h = ShardedHandle::create(dir.to_str().unwrap(), &config).unwrap();

    // A real corpus when the parity knobs are set, a synthetic one otherwise so
    // the test runs everywhere.
    let docs: Vec<(String, String, String)> = match (std::env::var("LUCE_ROOT"), std::env::var("LUCE_LIST")) {
        (Ok(root), Ok(list)) => {
            let max: usize = std::env::var("LUCE_MAX_DOCS").ok().and_then(|v| v.parse().ok()).unwrap_or(2000);
            std::fs::read_to_string(&list)
                .unwrap()
                .lines()
                .filter(|l| !l.is_empty())
                .take(max)
                .map(|rel| {
                    let content = std::fs::read(Path::new(&root).join(rel)).unwrap();
                    let ext = rel.rsplit('.').next().filter(|_| rel.contains('.')).unwrap_or("").to_string();
                    (rel.to_string(), String::from_utf8_lossy(&content).into_owned(), ext)
                })
                .collect()
        }
        _ => (0..1500)
            .map(|i| {
                (
                    format!("drivers/net/ethernet/intel/e1000_{i}.c"),
                    format!(
                        "static int e1000_probe_{i}(struct pci_dev *pdev)\n\
                         {{\n    void *buf = kmalloc(sizeof(struct e1000_adapter), GFP_KERNEL);\n\
                             if (!buf)\n        return -ENOMEM;\n    spin_lock_init(&adapter->lock_{i});\n\
                             netdev_info(netdev, \"probe {i}\\n\");\n    kfree(buf);\n    return 0;\n}}\n",
                        i = i
                    ),
                    "c".to_string(),
                )
            })
            .collect(),
    };

    for (i, (path, content, ext)) in docs.iter().enumerate() {
        h.add_document_json(
            i as u64,
            &serde_json::json!({"path": path, "content": content, "extension": ext}),
        )
        .unwrap();
        if (i + 1) % 500 == 0 {
            h.commit().unwrap();
        }
    }
    h.commit().unwrap();
    h.commit().unwrap();

    let versions = sfx_versions(&h);
    eprintln!("[luce-v3] {} docs, sfx versions {:?}, settings {:?}", h.num_docs(), versions, settings_of(&h));
    assert!(
        versions.iter().all(|v| *v == 3),
        "the source index must be v3 containers, got {versions:?}"
    );
    assert!(settings_of(&h).iter().all(|v| *v == sfx_version), "source settings");

    let panel: Vec<(&str, QueryConfig)> = vec![
        ("contains kmalloc", contains("content", "kmalloc")),
        ("contains spin_lock_init", contains("content", "spin_lock_init")),
        ("contains return -ENOMEM;", contains("content", "return -ENOMEM;")),
        ("path contains ethernet", contains("path", "ethernet")),
        ("extension c", contains("extension", "c")),
        (
            "startsWith netdev",
            QueryConfig { anchor_start: Some(true), ..contains("content", "netdev") },
        ),
        (
            "fuzzy kmallc",
            QueryConfig { distance: Some(1), ..contains("content", "kmallc") },
        ),
        (
            "regex spin_lock_[a-z]+",
            QueryConfig { pattern: Some("spin_lock_[a-z]+".into()), value: None, query_type: "regex".into(), ..contains("content", "") },
        ),
        ("no hit zzqqxx", contains("content", "zzqqxx")),
    ];

    let before = run_panel(&h, &panel);
    for (name, (count, _)) in &before {
        eprintln!("[luce-v3] source {name:28} {count:6} hits");
    }
    assert!(
        before.iter().filter(|(n, _)| n != "no hit zzqqxx").all(|(_, (c, _))| *c > 0),
        "the source index answers nothing — the panel is wrong, not the snapshot"
    );

    // ── Export ──────────────────────────────────────────────────────────
    let t = std::time::Instant::now();
    let blob = snapshot::export_to_snapshot(&h, &dir).unwrap();
    let on_disk: u64 = walk_bytes(&dir);
    eprintln!(
        "[luce-v3] snapshot {:.1} MB in {:.1}s (index on disk {:.1} MB)",
        blob.len() as f64 / 1e6,
        t.elapsed().as_secs_f64(),
        on_disk as f64 / 1e6
    );
    h.close().unwrap();

    // ── Import into a fresh directory ───────────────────────────────────
    let dest = std::env::temp_dir().join(format!("lucivy_luce_v{sfx_version}_dst"));
    let _ = std::fs::remove_dir_all(&dest);
    std::fs::create_dir_all(&dest).unwrap();
    let t = std::time::Instant::now();
    let h2 = snapshot::import_from_snapshot(&blob, &dest).unwrap();
    eprintln!(
        "[luce-v3] imported {} docs, {} shards, sfx versions {:?} in {:.1}s",
        h2.num_docs(),
        h2.num_shards(),
        sfx_versions(&h2),
        t.elapsed().as_secs_f64()
    );
    assert_eq!(h2.num_docs(), docs.len() as u64, "document count after import");
    assert!(sfx_versions(&h2).iter().all(|v| *v == 3), "imported index must stay v3 containers");
    assert!(settings_of(&h2).iter().all(|v| *v == sfx_version),
        "imported index must keep sfx_version {sfx_version}, got {:?}", settings_of(&h2));

    // ── Same answers ────────────────────────────────────────────────────
    let after = run_panel(&h2, &panel);
    for ((name, (c1, top1)), (_, (c2, top2))) in before.iter().zip(after.iter()) {
        assert_eq!(c1, c2, "{name}: {c1} hits before, {c2} after the round-trip");
        assert_eq!(
            top1, top2,
            "{name}: top-10 differs after the round-trip\n  before {top1:?}\n  after  {top2:?}"
        );
        eprintln!("[luce-v3] OK {name:28} {c1:6} hits, top-10 identical");
    }
    h2.close().unwrap();
}

fn walk_bytes(dir: &Path) -> u64 {
    let mut total = 0;
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            total += if p.is_dir() { walk_bytes(&p) } else { e.metadata().map(|m| m.len()).unwrap_or(0) };
        }
    }
    total
}
