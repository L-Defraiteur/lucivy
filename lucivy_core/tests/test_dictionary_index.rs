//! The shard dictionary (`sfx_version` 4) answers exactly like a v3 index:
//! same documents, same spans, on a panel of query kinds, through several
//! commits (ids minted, then found again) and the policy's merges; a
//! reopened index answers the same; and the files are what the design
//! says — no `.sfx` / `.termtexts` per segment, one generation per field.

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
        // Synthetic fallback: a vocabulary the kernel panel would find.
        let vocab = ["mutex_lock", "mutex_unlock", "spin_lock_irqsave", "sched_setscheduler", "printk", "schedule",
                     "register_device", "kmalloc(sizeof(*p), GFP_KERNEL)", "return -EINVAL;", "if (!ptr)", "struct file *f"];
        files.clear();
        for i in 0..max {
            let mut t = String::new();
            for j in 0..60 { t.push_str(vocab[(i * 7 + j * 13) % vocab.len()]); t.push_str(if j % 5 == 0 { "\n" } else { " " }); }
            files.push((format!("synthetic/{i}.c"), t));
        }
    }
    files
}

fn config(sfx_version: u8) -> SchemaConfig {
    serde_json::from_value(serde_json::json!({
        "fields": [
            {"name": "path", "type": "text", "stored": true},
            {"name": "content", "type": "text", "stored": true}
        ],
        "sfx_version": sfx_version
    })).unwrap()
}

fn build(files: &[(String, String)], sfx_version: u8, dir: &Path) -> LucivyHandle {
    // Three live generations at most: eight commits exercise two compactions.
    std::env::set_var("LUCIVY_DICT_MAX_GENERATIONS", "3");
    std::fs::create_dir_all(dir).unwrap();
    let mmap = ld_lucivy::directory::MmapDirectory::open(dir).unwrap();
    let handle = LucivyHandle::create(mmap, &config(sfx_version)).unwrap();
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
        Q { text: "mutex_lock", strict: false, distance: 0, anchor: false, exact: false, label: "relax" },
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

fn dir_bytes(dir: &Path) -> (u64, Vec<String>) {
    let mut names = Vec::new();
    let mut total = 0;
    let mut by_ext: std::collections::BTreeMap<String, (usize, u64)> = Default::default();
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let len = e.metadata().unwrap().len();
        let ext = name.rsplit('.').next().unwrap_or("").to_string();
        let key = if name.starts_with("dict-") { format!("dict.{ext}") } else { ext };
        let slot = by_ext.entry(key).or_insert((0, 0));
        slot.0 += 1; slot.1 += len;
        names.push(name);
        total += len;
    }
    names.sort();
    for (ext, (n, bytes)) in &by_ext {
        eprintln!("  {:<16} {:>4} files {:>10} bytes", ext, n, bytes);
    }
    (total, names)
}

#[test]
fn dictionary_index_answers_like_v3() {
    let files = corpus(300);
    eprintln!("corpus: {} files", files.len());
    let base: PathBuf = std::env::temp_dir().join(format!("lucivy-dict-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let h3 = build(&files, 3, &base.join("v3"));
    let h4 = build(&files, 4, &base.join("v4"));

    eprintln!("v3:");
    let (b3, n3) = dir_bytes(&base.join("v3"));
    eprintln!("v4:");
    let (b4, n4) = dir_bytes(&base.join("v4"));
    let seg_sfx = |names: &[String]| names.iter().filter(|n| !n.starts_with("dict-") && (n.ends_with(".sfx") || n.ends_with(".termtexts"))).count();
    let dict_files: Vec<&String> = n4.iter().filter(|n| n.starts_with("dict-")).collect();
    eprintln!("v3: {} files, {} bytes | v4: {} files, {} bytes, dictionary files {:?}", n3.len(), b3, n4.len(), b4, dict_files);
    assert!(seg_sfx(&n3) > 0, "a v3 index has .sfx and .termtexts per segment");
    assert_eq!(seg_sfx(&n4), 0, "a dictionary index has none per segment: {n4:?}");
    assert!(dict_files.iter().any(|n| n.ends_with(".sfx")) && dict_files.iter().any(|n| n.ends_with(".termtexts")),
        "one generation per field: {dict_files:?}");
    assert!(n4.iter().any(|n| n.ends_with(".gmap")), "segments carry a .gmap");
    eprintln!("size: v4 {b4} vs v3 {b3} ({:+.1} %)", 100.0 * (b4 as f64 - b3 as f64) / b3 as f64);
    let meta = std::fs::read_to_string(base.join("v4").join("meta.json")).unwrap();
    assert!(meta.contains("\"sfx_dictionary\""), "meta.json names the dictionary: {meta}");
    let live: Vec<u64> = h4.index.sfx_dictionary().unwrap().generations().to_vec();
    assert!(!live.is_empty() && live.len() <= 3, "at most three live generations after compaction: {live:?}");
    let on_disk: std::collections::BTreeSet<u64> = dict_files.iter()
        .filter_map(|n| n.trim_start_matches("dict-").split('.').next().and_then(|g| g.parse().ok())).collect();
    assert_eq!(on_disk, live.iter().copied().collect(), "only the live generations remain on disk");

    for q in panel() {
        let (d3, s3) = run(&h3, q);
        let (d4, s4) = run(&h4, q);
        assert!(!d3.is_empty(), "{} {}: the panel must find something in v3", q.text, q.label);
        assert_eq!(d3, d4, "{} {}: documents differ (v3 {} / v4 {})", q.text, q.label, d3.len(), d4.len());
        assert_eq!(s3, s4, "{} {}: spans differ (v3 {} / v4 {})", q.text, q.label, s3.len(), s4.len());
        eprintln!("{:<20} {:<10} {:>5} docs {:>6} spans — identical", q.text, q.label, d3.len(), s3.len());
    }

    // Reopen the dictionary index from disk and ask again.
    h4.close().unwrap();
    let reopened = LucivyHandle::open(ld_lucivy::directory::MmapDirectory::open(base.join("v4")).unwrap()).unwrap();
    for q in panel().into_iter().take(4) {
        let (d3, s3) = run(&h3, q);
        let (d4, s4) = run(&reopened, q);
        assert_eq!(d3, d4, "{} {} after reopen: documents differ", q.text, q.label);
        assert_eq!(s3, s4, "{} {} after reopen: spans differ", q.text, q.label);
    }
    h3.close().unwrap();
    reopened.close().unwrap();
    let _ = std::fs::remove_dir_all(&base);
}

/// Debug: the pieces of a dictionary lookup, one by one.
#[test]
fn dictionary_pieces() {
    let files = vec![
        ("a.c".to_string(), "int x = mutex_lock(&m); mutex_unlock(&m);".to_string()),
        ("b.c".to_string(), "spin_lock(&l); Mutex_lock(&m);".to_string()),
        ("c.c".to_string(), "printk(\"hello\"); schedule();".to_string()),
    ];
    let base: PathBuf = std::env::temp_dir().join(format!("lucivy-dict-pieces-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let h4 = build(&files, 4, &base.join("v4"));
    let content_f = h4.field("content").unwrap();
    let dict = h4.index.sfx_dictionary().expect("dictionary");
    eprintln!("dictionary generations {:?} next_ids {:?} fields {:?}", dict.generations(), dict.next_ids(), dict.meta().field_ids);
    let field = dict.field(content_f.field_id()).expect("content field in dictionary");
    let parents = field.sfx_reader().resolve_suffix("mutex_");
    eprintln!("dict parents of 'mutex_': {:?}", parents.iter().map(|p| (p.raw_ordinal, p.sti, p.own_len, &p.overlap[..2])).collect::<Vec<_>>());
    let tt = field.termtexts_reader().unwrap();
    for p in &parents { eprintln!("  id {} → text {:?} meta {:?}", p.raw_ordinal, tt.text(p.raw_ordinal as u32), tt.meta(p.raw_ordinal as u32)); }
    eprintln!("dict num_terms {} fst keys {}", tt.num_terms(), field.sfx_reader().num_suffix_terms());
    let searcher = h4.reader.searcher();
    for sr in searcher.segment_readers() {
        let gmap_bytes = sr.sfx_index_file("gmap", content_f).unwrap().read_bytes().unwrap();
        let gmap = ld_lucivy::suffix_fst::gmap::GmapReader::open(&gmap_bytes).unwrap();
        eprintln!("segment {:?}: {} docs, gmap {} globals: {:?}", sr.segment_id(), sr.num_docs(), gmap.len(), gmap.iter().take(12).collect::<Vec<_>>());
        let sp_bytes = sr.sfxpost_file(content_f).unwrap().read_bytes().unwrap();
        let sp = ld_lucivy::suffix_fst::sfxpost_v2::SfxPostReaderV2::open_owned(sp_bytes.clone()).unwrap();
        eprintln!("  sfxpost num_terms {}", sp.num_terms());
        for p in &parents {
            let local = gmap.local(p.raw_ordinal as u32);
            let entries = local.map(|l| sp.entries(l)).unwrap_or_default();
            eprintln!("  id {} → local {:?} → {} postings {:?}", p.raw_ordinal, local, entries.len(), entries.iter().map(|e| (e.doc_id, e.token_index)).collect::<Vec<_>>());
        }
        let sfx = sr.sfx_file(content_f).unwrap().read_bytes().unwrap();
        eprintln!("  segment sfx slice bytes {} (dictionary's: {})", sfx.len(), field.sfx.read_bytes().unwrap().len());
    }
    // The briques one by one, on the dictionary FST and the segment's files.
    {
        use ld_lucivy::suffix_fst::briques::{fst_walk, resolve};
        let reader = field.sfx_reader();
        let splits = fst_walk::falling_walk_chunks(reader, "mutex_lock");
        eprintln!("splits: {:?}", splits.iter().map(|s| (s.parent.raw_ordinal, s.parent.sti, s.query_consumed, s.overlap_validated)).collect::<Vec<_>>());
        let chains = fst_walk::cross_chunk_chain_v3(reader, "mutex_lock", false);
        eprintln!("chains: {:?}", chains.iter().map(|c| (c.first_sti, c.total_query_consumed, c.ordinals.iter().map(|a| a.explicit().to_vec()).collect::<Vec<_>>())).collect::<Vec<_>>());
        let cands = fst_walk::fst_candidates_v3(reader, "lock", true, true);
        eprintln!("cands 'lock' anchored: {:?}", cands.iter().map(|c| (c.raw_ordinal, c.sti, c.own_len)).collect::<Vec<_>>());
        for sr in searcher.segment_readers() {
            let pr = ld_lucivy::query::posting_resolver::build_resolver(sr, content_f).unwrap();
            let gmap_bytes = sr.sfx_index_file("gmap", content_f).unwrap().read_bytes().unwrap();
            let gmap = ld_lucivy::suffix_fst::gmap::GmapReader::open(&gmap_bytes).unwrap();
            let pm_bytes = sr.sfx_index_file("posmap", content_f).unwrap().read_bytes().unwrap();
            let pm = ld_lucivy::suffix_fst::posmap::PosMapReader::open(&pm_bytes).unwrap().with_gmap(gmap);
            for c in &chains {
                for &o in c.first_ids().iter() {
                    eprintln!("  head {} postings {:?}", o, pr.resolve(o).iter().map(|e| (e.doc_id, e.position)).collect::<Vec<_>>());
                    for e in pr.resolve(o) {
                        eprintln!("    posmap({}, {}) = {:?} ; chain next = {:?}", e.doc_id, e.position + 1, pm.ordinal_at(e.doc_id, e.position + 1), c.ordinals.get(1).map(|a| a.explicit().to_vec()));
                    }
                }
            }
            let matches = resolve::resolve_chains_v3_posmap(&chains, &*pr, None, &pm, None);
            eprintln!("  resolved matches: {}", matches.len());
        }
    }
    let (d, s) = run(&h4, Q { text: "mutex_lock", strict: true, distance: 0, anchor: false, exact: false, label: "strict" });
    eprintln!("query mutex_lock strict → docs {:?} spans {:?}", d, s);
    let (d, s) = run(&h4, Q { text: "mutex_lock", strict: false, distance: 0, anchor: false, exact: false, label: "relax" });
    eprintln!("query mutex_lock relax → docs {:?} spans {:?}", d, s);
    h4.close().unwrap();
    let _ = std::fs::remove_dir_all(&base);
}

/// The deferred fold (6 September 2026): a commit names its segments' pairs
/// and returns; a search right after it finds the new texts (it waits for
/// the background fold by default); once the writer has waited for its
/// background work, `meta.json` names generations only and no pair is left
/// on disk; and a reopen answers the same.
#[test]
fn deferred_fold_settles() {
    let files = corpus(400);
    let base: PathBuf = std::env::temp_dir().join(format!("lucivy-dict-fold-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let dir = base.join("v4");
    let h4 = build(&files, 4, &dir);

    // A search right after a commit, through the handle (which waits).
    {
        let mut guard = h4.writer.lock().unwrap();
        let w = guard.as_mut().unwrap();
        let mut doc = ld_lucivy::LucivyDocument::new();
        doc.add_u64(h4.field(NODE_ID_FIELD).unwrap(), 999_999);
        doc.add_text(h4.field("path").unwrap(), "zz/fresh.c");
        doc.add_text(h4.field("content").unwrap(), "int zzqq_fresh_token_after_commit(void);");
        w.add_document(doc).unwrap();
        w.commit().unwrap();
        drop(guard);
        h4.reader.reload().unwrap();
        let cfg = QueryConfig {
            query_type: "contains".into(),
            field: Some("content".into()),
            value: Some("zzqq_fresh_token".into()),
            strict_separators: Some(true),
            ..Default::default()
        };
        let hits = h4.search(&cfg, 10, None).unwrap();
        assert_eq!(hits.len(), 1, "the text minted by the last commit must be found right away");
    }

    // The writer closed after its background work: the disk names
    // generations only.
    {
        let mut guard = h4.writer.lock().unwrap();
        guard.take().unwrap().wait_merging_threads().unwrap();
    }
    let meta: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.join("meta.json")).unwrap()).unwrap();
    let dict = &meta["sfx_dictionary"];
    assert!(dict["pending_segments"].as_array().is_none_or(|p| p.is_empty()),
        "meta.json still names pending pairs: {}", dict["pending_segments"]);
    assert!(!dict["generations"].as_array().unwrap().is_empty());
    let (_, names) = dir_bytes(&dir);
    let pairs: Vec<&String> = names.iter().filter(|n| n.ends_with(".newsfx") || n.ends_with(".newtexts")).collect();
    assert!(pairs.is_empty(), "pairs left on disk after the fold: {pairs:?}");

    // A reopen from disk answers the panel like the live handle.
    let h3 = build(&files, 3, &base.join("v3"));
    let reopened = LucivyHandle::open(ld_lucivy::directory::MmapDirectory::open(&dir).unwrap()).unwrap();
    for q in panel() {
        let (d3, s3) = run(&h3, q);
        let (d4, s4) = run(&reopened, q);
        // The fresh document exists only in the v4 index.
        let d4: HashSet<u64> = d4.into_iter().filter(|&d| d != 999_999).collect();
        let s4: HashSet<(u64, usize, usize)> = s4.into_iter().filter(|(d, _, _)| *d != 999_999).collect();
        assert_eq!(d3, d4, "{} [{}] documents", q.text, q.label);
        assert_eq!(s3, s4, "{} [{}] spans", q.text, q.label);
    }
    let _ = std::fs::remove_dir_all(&base);
}
