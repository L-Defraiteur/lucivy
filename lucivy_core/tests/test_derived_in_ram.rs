//! `derived_in_ram`: an index that does not write `.posmap`, `.word_pos_map`
//! and `.sibling_v3` answers exactly like one that does — same documents,
//! same spans, on a panel of query kinds, through several commits and the
//! policy's merges, in v3 and with the shard dictionary; a reopened index
//! answers the same; and the files are what the option says: none of the
//! three on disk, the setting in `meta.json`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use lucivy_core::handle::{LucivyHandle, NODE_ID_FIELD};
use lucivy_core::query::{self, QueryConfig, SchemaConfig};

const RX: u8 = 255;

fn corpus(max: usize) -> Vec<(String, String)> {
    let root = Path::new("/tmp/lucivy-cmp");
    let mut files = Vec::new();
    fn walk(dir: &Path, files: &mut Vec<(String, String)>, max: usize) {
        if files.len() >= max { return; }
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(|e| e.path());
        for e in entries {
            if files.len() >= max { return; }
            let p = e.path();
            if p.is_dir() { walk(&p, files, max); continue; }
            let ok_ext = p.extension().and_then(|x| x.to_str()).is_some_and(|x| matches!(x, "c" | "h" | "rst" | "txt"));
            if !ok_ext { continue; }
            let Ok(meta) = e.metadata() else { continue };
            if meta.len() == 0 || meta.len() > 60_000 { continue; }
            if let Ok(text) = std::fs::read_to_string(&p) {
                files.push((p.to_string_lossy().to_string(), text));
            }
        }
    }
    if root.exists() {
        walk(root, &mut files, max);
    }
    if files.len() < 50 {
        let vocab = ["mutex_lock", "mutex_unlock", "spin_lock_irqsave", "sched_setscheduler", "printk", "schedule",
                     "register_device", "kmalloc(sizeof(*p), GFP_KERNEL)", "return -EINVAL;", "if (!ptr)", "struct file *f",
                     "可以理可以理可以理解。", "déjà vu"];
        files.clear();
        for i in 0..max {
            let mut t = String::new();
            for j in 0..60 { t.push_str(vocab[(i * 7 + j * 13) % vocab.len()]); t.push_str(if j % 5 == 0 { "\n" } else { " " }); }
            files.push((format!("synthetic/{i}.c"), t));
        }
    }
    files
}

fn config(sfx_version: u8, derived_in_ram: bool) -> SchemaConfig {
    serde_json::from_value(serde_json::json!({
        "fields": [
            {"name": "path", "type": "text", "stored": true},
            {"name": "content", "type": "text", "stored": true}
        ],
        "sfx_version": sfx_version,
        "derived_in_ram": derived_in_ram
    })).unwrap()
}

fn build(files: &[(String, String)], sfx_version: u8, derived_in_ram: bool, dir: &Path) -> LucivyHandle {
    std::fs::create_dir_all(dir).unwrap();
    let mmap = ld_lucivy::directory::MmapDirectory::open(dir).unwrap();
    let handle = LucivyHandle::create(mmap, &config(sfx_version, derived_in_ram)).unwrap();
    let path_f = handle.field("path").unwrap();
    let content_f = handle.field("content").unwrap();
    let nid_f = handle.field(NODE_ID_FIELD).unwrap();
    {
        let mut guard = handle.writer.lock().unwrap();
        let w = guard.as_mut().unwrap();
        for (i, (path, content)) in files.iter().enumerate() {
            let mut doc = ld_lucivy::LucivyDocument::new();
            doc.add_u64(nid_f, i as u64);
            doc.add_text(path_f, path);
            doc.add_text(content_f, content);
            w.add_document(doc).unwrap();
            if (i + 1) % 40 == 0 {
                w.commit().unwrap();
            }
        }
        w.commit().unwrap();
        w.drain_merges().unwrap();
        w.commit().unwrap();
    }
    handle.reader.reload().unwrap();
    handle
}

#[derive(Clone, Copy)]
struct Q { text: &'static str, strict: bool, distance: u8, anchor: bool, exact: bool, label: &'static str }

fn panel() -> Vec<Q> {
    vec![
        Q { text: "mutex_lock", strict: true, distance: 0, anchor: false, exact: false, label: "strict" },
        Q { text: "mutex lock", strict: false, distance: 0, anchor: false, exact: false, label: "relax" },
        Q { text: "spin_lock", strict: true, distance: 0, anchor: false, exact: false, label: "strict" },
        Q { text: "sched", strict: true, distance: 0, anchor: true, exact: true, label: "term" },
        Q { text: "printk", strict: true, distance: 0, anchor: true, exact: false, label: "sw" },
        Q { text: "return", strict: false, distance: 0, anchor: false, exact: false, label: "relax" },
        Q { text: "if (", strict: true, distance: 0, anchor: false, exact: false, label: "strict" },
        Q { text: "schdule", strict: false, distance: 1, anchor: false, exact: false, label: "fz1" },
        Q { text: "regsiter", strict: false, distance: 2, anchor: false, exact: false, label: "fz2" },
        Q { text: "spin_lock_[a-z]+", strict: true, distance: RX, anchor: false, exact: false, label: "rx" },
        Q { text: "Mutex", strict: false, distance: 0, anchor: false, exact: false, label: "relax-case" },
    ]
}

fn run(handle: &LucivyHandle, q: Q) -> (HashSet<u64>, HashSet<(u64, usize, usize)>) {
    let sink = Arc::new(ld_lucivy::query::HighlightSink::new());
    let cfg = QueryConfig {
        query_type: "contains".into(),
        field: Some("content".into()),
        value: Some(q.text.into()),
        strict_separators: Some(q.strict),
        distance: if q.distance > 0 && q.distance != RX { Some(q.distance) } else { None },
        regex: if q.distance == RX { Some(true) } else { None },
        anchor_start: if q.anchor { Some(true) } else { None },
        exact_match: if q.exact { Some(true) } else { None },
        ..Default::default()
    };
    let query = query::build_query(&cfg, &handle.schema, &handle.index, Some(Arc::clone(&sink))).unwrap();
    let searcher = handle.reader.searcher();
    let collector = ld_lucivy::collector::TopDocs::with_limit(100_000).order_by_score();
    let results = searcher.search(&*query, &collector).unwrap();
    let nid_f = handle.field(NODE_ID_FIELD).unwrap();
    let mut docs = HashSet::new();
    let mut spans = HashSet::new();
    for (_, addr) in &results {
        let doc = searcher.doc::<ld_lucivy::LucivyDocument>(*addr).unwrap();
        use ld_lucivy::schema::document::Value;
        let nid = doc.field_values().find(|(f, _)| *f == nid_f)
            .and_then(|(_, v)| v.as_value().as_u64()).unwrap();
        docs.insert(nid);
        let seg_id = searcher.segment_reader(addr.segment_ord).segment_id();
        if let Some(hl) = sink.get(seg_id, addr.doc_id) {
            if let Some(offsets) = hl.get("content") {
                for [s, e] in offsets { spans.insert((nid, *s, *e)); }
            }
        }
    }
    (docs, spans)
}

fn files_of(dir: &Path) -> (u64, Vec<String>) {
    let mut names = Vec::new();
    let mut total = 0;
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        names.push(e.file_name().to_string_lossy().to_string());
        total += e.metadata().unwrap().len();
    }
    names.sort();
    (total, names)
}

fn derived_on_disk(names: &[String]) -> usize {
    names.iter().filter(|n| n.ends_with(".posmap") || n.ends_with(".word_pos_map") || n.ends_with(".sibling_v3")).count()
}

fn compare(reference: &LucivyHandle, candidate: &LucivyHandle, what: &str) {
    for q in panel() {
        let (d0, s0) = run(reference, q);
        let (d1, s1) = run(candidate, q);
        assert!(!d0.is_empty(), "{} {}: the panel must find something", q.text, q.label);
        assert_eq!(d0, d1, "{what}: {} {}: documents differ ({} / {})", q.text, q.label, d0.len(), d1.len());
        assert_eq!(s0, s1, "{what}: {} {}: spans differ ({} / {})", q.text, q.label, s0.len(), s1.len());
        eprintln!("{what:<22} {:<20} {:<10} {:>5} docs {:>6} spans — identical", q.text, q.label, d0.len(), s0.len());
    }
}

#[test]
fn derived_in_ram_answers_like_the_files() {
    let files = corpus(300);
    eprintln!("corpus: {} files", files.len());
    let base: PathBuf = std::env::temp_dir().join(format!("lucivy-derived-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let h3 = build(&files, 3, false, &base.join("v3"));
    let h3r = build(&files, 3, true, &base.join("v3-ram"));
    let h4r = build(&files, 4, true, &base.join("v4-ram"));

    let (b3, n3) = files_of(&base.join("v3"));
    let (b3r, n3r) = files_of(&base.join("v3-ram"));
    let (b4r, n4r) = files_of(&base.join("v4-ram"));
    assert!(derived_on_disk(&n3) > 0, "a plain index writes the derived sidecars");
    assert_eq!(derived_on_disk(&n3r), 0, "derived_in_ram writes none of the three: {n3r:?}");
    assert_eq!(derived_on_disk(&n4r), 0, "derived_in_ram with the dictionary writes none either: {n4r:?}");
    assert!(b3r < b3, "smaller on disk: {b3r} against {b3}");
    eprintln!("size: v3 {b3} B, v3 derived_in_ram {b3r} B ({:+.1} %), dictionary + derived_in_ram {b4r} B",
        100.0 * (b3r as f64 - b3 as f64) / b3 as f64);
    let meta: String = std::fs::read_to_string(base.join("v3-ram").join("meta.json")).unwrap()
        .chars().filter(|c| !c.is_whitespace()).collect();
    assert!(meta.contains("\"derived_in_ram\":true"), "meta.json carries the setting: {meta}");
    let meta = std::fs::read_to_string(base.join("v3").join("meta.json")).unwrap();
    assert!(!meta.contains("derived_in_ram"), "an index without the option does not name it: {meta}");

    compare(&h3, &h3r, "v3 derived_in_ram");
    compare(&h3, &h4r, "dictionary + in RAM");

    // Reopen from disk: the setting is read back, the sidecars rebuilt again.
    h3r.close().unwrap();
    h4r.close().unwrap();
    let r3 = LucivyHandle::open(ld_lucivy::directory::MmapDirectory::open(base.join("v3-ram")).unwrap()).unwrap();
    let r4 = LucivyHandle::open(ld_lucivy::directory::MmapDirectory::open(base.join("v4-ram")).unwrap()).unwrap();
    compare(&h3, &r3, "v3 in RAM, reopened");
    compare(&h3, &r4, "dictionary, reopened");
    h3.close().unwrap();
    r3.close().unwrap();
    r4.close().unwrap();
    let _ = std::fs::remove_dir_all(&base);
}
