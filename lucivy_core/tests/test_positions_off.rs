//! `positions: false` (4.1): an index whose postings keep each token's
//! documents and term frequencies, not its positions.
//!
//! What this file pins, layer by layer as the chantier goes
//! (`docs/08-09-2026/01-chantier-positions-optionnelles.md`):
//! - the files: no `.posmap`, `.word_pos_map`, `.sibling_v3` on disk, the
//!   setting in `meta.json`, a smaller index;
//! - the merges: they carry the frequencies — the total number of token
//!   occurrences, which no segmentation changes, is the same as in an
//!   index built with positions from the same documents;
//! - the configuration: refused with `derived_in_ram`, with an unstored
//!   text field, with the v2 engine;
//! - the answers: the same documents and the same byte spans as the index
//!   with positions, query kind by query kind as the regimes land.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use lucivy_core::handle::{LucivyHandle, NODE_ID_FIELD};
use lucivy_core::query::{self, QueryConfig, SchemaConfig};

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

fn config(sfx_version: u8, positions: bool) -> SchemaConfig {
    serde_json::from_value(serde_json::json!({
        "fields": [
            {"name": "path", "type": "text", "stored": true},
            {"name": "content", "type": "text", "stored": true}
        ],
        "sfx_version": sfx_version,
        "positions": positions
    })).unwrap()
}

fn build(files: &[(String, String)], sfx_version: u8, positions: bool, dir: &Path) -> LucivyHandle {
    std::fs::create_dir_all(dir).unwrap();
    let mmap = ld_lucivy::directory::MmapDirectory::open(dir).unwrap();
    let handle = LucivyHandle::create(mmap, &config(sfx_version, positions)).unwrap();
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

/// `(Σ documents, Σ occurrences)` of the content field's chunk postings
/// (`sfxpost`) or word postings (`word_sfxpost`), over every segment and
/// every ordinal. The occurrences are the tokens of the corpus: no
/// segmentation or merge changes their total.
fn occurrences(handle: &LucivyHandle, ext: &str) -> (u64, u64) {
    let field = handle.field("content").unwrap();
    let searcher = handle.reader.searcher();
    let (mut docs, mut occ) = (0u64, 0u64);
    for seg in searcher.segment_readers() {
        let bytes = seg.sfx_index_file(ext, field).unwrap().read_bytes().unwrap();
        let mut count = |_: u32, tf: u32| { docs += 1; occ += tf as u64; };
        if ext == "sfxpost" {
            let r = ld_lucivy::suffix_fst::sfxpost_v2::SfxPostReaderV2::open_owned(bytes).unwrap();
            for o in 0..r.num_terms() { r.for_each_doc(o, &mut count); }
        } else {
            let r = ld_lucivy::suffix_fst::word_sfxpost::WordSfxPostReader::open(&bytes).unwrap();
            for o in 0..r.num_ordinals() { r.for_each_doc(o, &mut count); }
        }
    }
    (docs, occ)
}

#[test]
fn positions_off_writes_documents_and_frequencies_only() {
    let files = corpus(300);
    eprintln!("corpus: {} files", files.len());
    let base: PathBuf = std::env::temp_dir().join(format!("lucivy-positions-off-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    for sfx_version in [3u8, 4] {
        let with = build(&files, sfx_version, true, &base.join(format!("v{sfx_version}")));
        let without = build(&files, sfx_version, false, &base.join(format!("v{sfx_version}-nopos")));
        let segs = without.reader.searcher().segment_readers().len();

        let (b_with, n_with) = files_of(&base.join(format!("v{sfx_version}")));
        let (b_without, n_without) = files_of(&base.join(format!("v{sfx_version}-nopos")));
        assert!(derived_on_disk(&n_with) > 0, "v{sfx_version}: an index with positions writes the derived sidecars");
        assert_eq!(derived_on_disk(&n_without), 0, "v{sfx_version}: positions: false writes none of the three: {n_without:?}");
        assert!(b_without < b_with, "v{sfx_version}: smaller on disk: {b_without} against {b_with}");
        eprintln!("v{sfx_version}: {segs} segments; {b_with} B with positions, {b_without} B without ({:+.1} %)",
            100.0 * (b_without as f64 - b_with as f64) / b_with as f64);

        let meta: String = std::fs::read_to_string(base.join(format!("v{sfx_version}-nopos")).join("meta.json")).unwrap()
            .chars().filter(|c| !c.is_whitespace()).collect();
        assert!(meta.contains("\"positions\":false"), "meta.json carries the setting: {meta}");
        let meta = std::fs::read_to_string(base.join(format!("v{sfx_version}")).join("meta.json")).unwrap();
        assert!(!meta.contains("\"positions\""), "an index with positions does not name the setting: {meta}");

        for ext in ["sfxpost", "word_sfxpost"] {
            let (d0, o0) = occurrences(&with, ext);
            let (d1, o1) = occurrences(&without, ext);
            assert!(o0 > 0, "v{sfx_version} {ext}: the corpus has occurrences");
            assert_eq!(o0, o1, "v{sfx_version} {ext}: the frequencies survive the commits and the merges");
            eprintln!("v{sfx_version} {ext:<12} {o0} occurrences in both; Σ documents {d0} / {d1} (depends on the segmentation)");
        }

        // A reopened index keeps its layout.
        drop(without);
        let reopened = LucivyHandle::open(ld_lucivy::directory::MmapDirectory::open(base.join(format!("v{sfx_version}-nopos"))).unwrap()).unwrap();
        assert!(!reopened.index.settings().positions, "v{sfx_version}: reopened without positions");
        assert!(reopened.index.settings().skips_derived_files());
    }
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn positions_off_is_refused_where_it_cannot_answer() {
    let create = |json: serde_json::Value| -> Result<(), String> {
        let config: SchemaConfig = serde_json::from_value(json).unwrap();
        LucivyHandle::create(ld_lucivy::directory::RamDirectory::default(), &config).map(|_| ())
    };
    let err = create(serde_json::json!({
        "fields": [{"name": "content", "type": "text", "stored": true}],
        "positions": false, "derived_in_ram": true
    })).unwrap_err();
    assert!(err.contains("derived_in_ram"), "{err}");
    let err = create(serde_json::json!({
        "fields": [{"name": "content", "type": "text", "stored": false}],
        "positions": false
    })).unwrap_err();
    assert!(err.contains("must be stored"), "{err}");
    let err = create(serde_json::json!({
        "fields": [{"name": "content", "type": "text"}],
        "positions": false
    })).unwrap_err();
    assert!(err.contains("must be stored"), "unset means not stored: {err}");
    let err = create(serde_json::json!({
        "fields": [{"name": "content", "type": "text", "stored": true}],
        "positions": false, "sfx_version": 2
    })).unwrap_err();
    assert!(err.contains("sfx_version 3 or 4"), "{err}");
    create(serde_json::json!({
        "fields": [{"name": "content", "type": "text", "stored": true}],
        "positions": false
    })).expect("a stored text field without positions is fine");
}

const RX: u8 = 255;

#[derive(Clone, Copy)]
struct Q { text: &'static str, strict: bool, distance: u8, anchor: bool, exact: bool, label: &'static str, jw: Option<f32> }

/// The literal kinds (step 2): strict, relaxed, word start, whole word,
/// separators inside the needle, case, two characters.
fn literal_panel() -> Vec<Q> {
    vec![
        Q { text: "mutex_lock", strict: true, distance: 0, anchor: false, exact: false, label: "strict", jw: None },
        Q { text: "mutex lock", strict: false, distance: 0, anchor: false, exact: false, label: "relax", jw: None },
        Q { text: "spin_lock", strict: true, distance: 0, anchor: false, exact: false, label: "strict", jw: None },
        Q { text: "spinlock", strict: false, distance: 0, anchor: false, exact: false, label: "relax", jw: None },
        Q { text: "sched", strict: true, distance: 0, anchor: true, exact: true, label: "term", jw: None },
        Q { text: "printk", strict: true, distance: 0, anchor: true, exact: false, label: "sw", jw: None },
        Q { text: "return", strict: false, distance: 0, anchor: false, exact: false, label: "relax", jw: None },
        Q { text: "return -ENOMEM;", strict: true, distance: 0, anchor: false, exact: false, label: "strict", jw: None },
        Q { text: "if (", strict: true, distance: 0, anchor: false, exact: false, label: "strict", jw: None },
        Q { text: "->next", strict: true, distance: 0, anchor: false, exact: false, label: "strict", jw: None },
        Q { text: "Mutex", strict: false, distance: 0, anchor: false, exact: false, label: "relax-case", jw: None },
        Q { text: "de", strict: true, distance: 0, anchor: false, exact: false, label: "two chars", jw: None },
        Q { text: "pin_loc", strict: true, distance: 0, anchor: false, exact: false, label: "mid-token", jw: None },
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
        fuzzy_metric: q.jw.map(|_| "jaro_winkler".to_string()),
        min_similarity: q.jw,
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

fn compare(reference: &LucivyHandle, candidate: &LucivyHandle, panel: &[Q], what: &str) {
    for &q in panel {
        let (d0, s0) = run(reference, q);
        let (d1, s1) = run(candidate, q);
        assert!(!d0.is_empty(), "{} {}: the panel must find something", q.text, q.label);
        assert_eq!(d0, d1, "{what}: {} {}: documents differ ({} / {})", q.text, q.label, d0.len(), d1.len());
        assert_eq!(s0, s1, "{what}: {} {}: spans differ ({} / {})", q.text, q.label, s0.len(), s1.len());
        eprintln!("{what:<16} {:<16} {:<10} {:>5} docs {:>6} spans — identical", q.text, q.label, d0.len(), s0.len());
    }
}

#[test]
fn positions_off_answers_literals_like_positions() {
    let files = corpus(300);
    eprintln!("corpus: {} files", files.len());
    let base: PathBuf = std::env::temp_dir().join(format!("lucivy-positions-off-answers-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    for sfx_version in [3u8, 4] {
        let with = build(&files, sfx_version, true, &base.join(format!("v{sfx_version}")));
        let without = build(&files, sfx_version, false, &base.join(format!("v{sfx_version}-nopos")));
        compare(&with, &without, &literal_panel(), &format!("v{sfx_version} no positions"));
    }
    let _ = std::fs::remove_dir_all(&base);
}

/// Fuzzy (Levenshtein at one and two edits, across token boundaries,
/// Jaro-Winkler) and regex (with a literal, bounded or not, and without).
fn fuzzy_regex_panel() -> Vec<Q> {
    let q = |text, distance, label| Q { text, strict: false, distance, anchor: false, exact: false, label, jw: None };
    vec![
        q("schdule", 1, "fz1"),
        q("regsiter", 2, "fz2"),
        q("spinlokc", 2, "fz2 across"),
        q("mutx_lock", 1, "fz1 across"),
        Q { jw: Some(0.9), ..q("schdule", 1, "jw1") },
        Q { strict: true, ..q("spin_lock_[a-z]+", RX, "rx") },
        Q { strict: true, ..q("/\\*[^*]*\\*/", RX, "rx unbounded") },
        Q { strict: true, ..q("[0-9]{4}", RX, "rx no literal") },
    ]
}

/// The tier a fuzzy search gives each document, through the score's order:
/// the documents grouped by score, in order.
fn ranked(handle: &LucivyHandle, q: Q) -> Vec<(u64, i64)> {
    let cfg = QueryConfig {
        query_type: "contains".into(),
        field: Some("content".into()),
        value: Some(q.text.into()),
        strict_separators: Some(q.strict),
        distance: if q.distance > 0 && q.distance != RX { Some(q.distance) } else { None },
        regex: if q.distance == RX { Some(true) } else { None },
        fuzzy_metric: q.jw.map(|_| "jaro_winkler".to_string()),
        min_similarity: q.jw,
        ..Default::default()
    };
    let query = query::build_query(&cfg, &handle.schema, &handle.index, None).unwrap();
    let searcher = handle.reader.searcher();
    let results = searcher.search(&*query, &ld_lucivy::collector::TopDocs::with_limit(100_000).order_by_score()).unwrap();
    let nid_f = handle.field(NODE_ID_FIELD).unwrap();
    let mut out: Vec<(u64, i64)> = results.iter().map(|(score, addr)| {
        let doc = searcher.doc::<ld_lucivy::LucivyDocument>(*addr).unwrap();
        use ld_lucivy::schema::document::Value;
        let nid = doc.field_values().find(|(f, _)| *f == nid_f).and_then(|(_, v)| v.as_value().as_u64()).unwrap();
        (nid, (*score * 1000.0).round() as i64)
    }).collect();
    out.sort();
    out
}

#[test]
fn positions_off_answers_fuzzy_and_regex_like_positions() {
    let files = corpus(300);
    eprintln!("corpus: {} files", files.len());
    let base: PathBuf = std::env::temp_dir().join(format!("lucivy-positions-off-fuzzy-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    for sfx_version in [3u8, 4] {
        let with = build(&files, sfx_version, true, &base.join(format!("v{sfx_version}")));
        let without = build(&files, sfx_version, false, &base.join(format!("v{sfx_version}-nopos")));
        compare(&with, &without, &fuzzy_regex_panel(), &format!("v{sfx_version} no positions"));
        // Same scores, tiers included: a fuzzy document ranks where it did.
        for q in fuzzy_regex_panel().into_iter().filter(|q| q.distance != RX) {
            assert_eq!(ranked(&with, q), ranked(&without, q), "v{sfx_version} {} {}: scores differ", q.text, q.label);
        }
    }
    let _ = std::fs::remove_dir_all(&base);
}
