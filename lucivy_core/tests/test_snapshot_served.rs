//! Serving a LUCE snapshot without extracting it.
//!
//! The extracting path (`import_from_snapshot`) holds the blob and the files
//! at once: 4.6 GB to open a 2.3 GB index, against the 4 GB WebAssembly can
//! address. `ShardedHandle::open_snapshot` keeps the blob and serves slices of
//! it. This asserts the two answer identically — counts, top ids, scores to
//! the bit, span counts — and that the served handle refuses to be written to.
//!
//! With a real corpus:
//!
//!   SERVED_ROOT=/tmp/linux-bench SERVED_LIST=/tmp/corpus_indexed.list \
//!   SERVED_MAX_DOCS=3000 \
//!   cargo test --release -p lucivy-core --test test_snapshot_served -- --nocapture

use lucivy_core::query::{QueryConfig, SchemaConfig};
use lucivy_core::sharded_handle::ShardedHandle;
use lucivy_core::snapshot;
use std::path::Path;

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
                    use ld_lucivy::schema::Value;
                    let id = hit.doc.get_first(nid).and_then(|v| v.as_u64()).unwrap_or(u64::MAX);
                    let spans: usize = hit.highlights.values().map(|v| v.len()).sum();
                    (id, hit.score, spans)
                })
                .collect();
            (name.to_string(), (hits.len(), top))
        })
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
fn snapshot_served_answers_like_the_index_it_came_from() {
    let config: SchemaConfig = serde_json::from_value(serde_json::json!({
        "fields": [
            {"name": "path", "type": "text"},
            {"name": "content", "type": "text"},
            {"name": "extension", "type": "text"}
        ],
        "shards": 4
    }))
    .unwrap();

    let dir = std::env::temp_dir().join("lucivy_served_src");
    let _ = std::fs::remove_dir_all(&dir);
    let h = ShardedHandle::create(dir.to_str().unwrap(), &config).unwrap();

    let docs: Vec<(String, String, String)> =
        match (std::env::var("SERVED_ROOT"), std::env::var("SERVED_LIST")) {
            (Ok(root), Ok(list)) => {
                let max: usize = std::env::var("SERVED_MAX_DOCS")
                    .ok().and_then(|v| v.parse().ok()).unwrap_or(2000);
                std::fs::read_to_string(&list).unwrap().lines()
                    .filter(|l| !l.is_empty()).take(max)
                    .map(|rel| {
                        let content = std::fs::read(Path::new(&root).join(rel)).unwrap();
                        let ext = rel.rsplit('.').next().filter(|_| rel.contains('.')).unwrap_or("").to_string();
                        (rel.to_string(), String::from_utf8_lossy(&content).into_owned(), ext)
                    })
                    .collect()
            }
            _ => (0..1500)
                .map(|i| (
                    format!("drivers/net/ethernet/intel/e1000_{i}.c"),
                    format!(
                        "static int e1000_probe_{i}(struct pci_dev *pdev)\n{{\n\
                         	void *buf = kmalloc(sizeof(struct e1000_adapter), GFP_KERNEL);\n\
                         	if (!buf)\n		return -ENOMEM;\n\
                         	spin_lock_init(&adapter->lock_{i});\n\
                         	netdev_info(netdev, \"probe {i}\\n\");\n	kfree(buf);\n	return 0;\n}}\n"
                    ),
                    "c".to_string(),
                ))
                .collect(),
        };

    for (i, (path, content, ext)) in docs.iter().enumerate() {
        h.add_document_json(
            i as u64,
            &serde_json::json!({"path": path, "content": content, "extension": ext}),
        ).unwrap();
        if (i + 1) % 500 == 0 {
            h.commit().unwrap();
        }
    }
    h.commit().unwrap();
    h.commit().unwrap();

    let panel: Vec<(&str, QueryConfig)> = vec![
        ("contains kmalloc", contains("content", "kmalloc")),
        ("contains spin_lock_init", contains("content", "spin_lock_init")),
        ("contains return -ENOMEM;", contains("content", "return -ENOMEM;")),
        ("path contains ethernet", contains("path", "ethernet")),
        ("extension c", contains("extension", "c")),
        ("startsWith netdev", QueryConfig { anchor_start: Some(true), ..contains("content", "netdev") }),
        ("fuzzy kmallc", QueryConfig { distance: Some(1), ..contains("content", "kmallc") }),
        (
            "regex spin_lock_[a-z]+",
            QueryConfig {
                pattern: Some("spin_lock_[a-z]+".into()), value: None,
                query_type: "regex".into(), ..contains("content", "")
            },
        ),
        ("no hit zzqqxx", contains("content", "zzqqxx")),
    ];

    let before = run_panel(&h, &panel);
    let blob = snapshot::export_to_snapshot(&h, &dir).unwrap();
    eprintln!("[served] snapshot {:.1} MB for {} documents", blob.len() as f64 / 1e6, h.num_docs());
    h.close().unwrap();

    // ── Served from the blob, nothing written out ───────────────────────
    let bytes = ld_lucivy::directory::OwnedBytes::new(blob.clone());
    let served = ShardedHandle::open_snapshot(bytes).unwrap();
    eprintln!(
        "[served] {} docs, {} shards, residency {:?}",
        served.num_docs(), served.num_shards(), served.residency()
    );
    assert_eq!(served.num_docs(), docs.len() as u64, "document count");

    let after = run_panel(&served, &panel);
    for ((name, (c1, top1)), (_, (c2, top2))) in before.iter().zip(after.iter()) {
        assert_eq!(c1, c2, "{name}: {c1} hits from the index, {c2} from its snapshot");
        assert_eq!(top1, top2, "{name}: top-10 differs\n  index    {top1:?}\n  snapshot {top2:?}");
        eprintln!("[served] OK {name:28} {c1:6} hits, top-10 identical");
    }

    // The point of the exercise: the index is not a second copy of the blob.
    // The files are slices of it, so measuring the index measures the blob —
    // which is what the residency decision needs, and it works unchanged on a
    // served snapshot.
    let measured = served.index_bytes() as usize;
    assert!(
        measured > 0 && measured <= blob.len(),
        "a served index measures the blob it slices: {measured} bytes against a {} byte snapshot",
        blob.len()
    );
    eprintln!(
        "[served] index {} bytes of a {} byte snapshot ({:.0} % of the blob is live)",
        measured, blob.len(), measured as f64 * 100.0 / blob.len() as f64
    );
    // What the difference is made of, so a snapshot that carries dead segments
    // is visible rather than merely large.
    {
        let manifest = lucistore::snapshot::read_manifest(&blob).unwrap();
        let mut live: std::collections::HashSet<String> = std::collections::HashSet::new();
        for i in 0..served.num_shards() {
            let Some(sh) = served.shard(i) else { continue };
            let v = sh.index.settings().sfx_version;
            if let Ok(metas) = sh.index.searchable_segment_metas() {
                for p in metas.iter().flat_map(|m| m.list_files_for(v)) {
                    live.insert(p.to_string_lossy().into_owned());
                }
            }
        }
        let mut dead = 0usize;
        let mut dead_n = 0usize;
        for (_, entries) in &manifest.indexes {
            for e in entries {
                if !live.contains(&e.name) && e.name != "meta.json" {
                    dead += e.len;
                    dead_n += 1;
                }
            }
        }
        eprintln!("[served] {dead_n} files, {dead} bytes in the snapshot belong to no searchable segment");
    }

    assert!(
        blob.len() - measured < blob.len() / 10,
        "the snapshot should carry the index and little else: {measured} live bytes \
         of {} — anything more is segments no query can reach", blob.len()
    );

    // Read-only: a snapshot is served, not indexed into. The write is queued
    // through the pipeline, so the commit is where it must fail — and `close`
    // commits too, which is why it is not expected to succeed either.
    let _ = served.add_document_json(
        999_999, &serde_json::json!({"path": "x", "content": "y", "extension": "z"}));
    assert!(served.commit().is_err(), "committing into a served snapshot must fail");
    let _ = served.close();
}
