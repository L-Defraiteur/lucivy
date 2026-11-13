//! Reproduce the playground indexation flow natively.
//!
//! Indexes the rag3db repo files exactly like the WASM playground:
//! - Same TEXT_EXTENSIONS filter
//! - Same isBinaryContent check (null byte in first 512 bytes)
//! - Same MAX_FILE_SIZE = 100KB
//! - Same COMMIT_EVERY = 200 docs
//! - No drain_merges (WASM doesn't call it)
//! - Fields: path (text, stored, not indexed) + content (text, stored, indexed)
//!
//! The repo root defaults to the rag3db parent directory (packages/rag3db/).
//! Set RAG3DB_ROOT env var to override.

use lucivy_core::handle::LucivyHandle;
use lucivy_core::query::{self, QueryConfig, SchemaConfig};
use lucivy_core::directory::StdFsDirectory;
use std::sync::Arc;

const COMMIT_EVERY: usize = 200;
const MAX_FILE_SIZE: u64 = 100_000;

/// Same extensions as the playground's TEXT_EXTENSIONS set.
const TEXT_EXTENSIONS: &[&str] = &[
    "txt", "md", "rs", "py", "js", "ts", "jsx", "tsx", "json", "toml",
    "yaml", "yml", "html", "htm", "css", "scss", "less", "go", "java",
    "c", "cpp", "cc", "h", "hpp", "rb", "sh", "bash", "zsh", "fish",
    "sql", "xml", "csv", "tsv", "r", "swift", "kt", "scala",
    "lua", "vim", "el", "ex", "exs", "erl", "hs", "ml", "mli",
    "clj", "lisp", "php", "pl", "pm", "tcl", "awk", "sed",
    "makefile", "cmake", "dockerfile", "gitignore", "env",
    "cfg", "ini", "conf", "properties", "lock",
];

/// Same as playground's isTextFilename
fn is_text_filename(name: &str) -> bool {
    let lower = name.to_lowercase();
    if let Some(dot_pos) = lower.rfind('.') {
        let ext = &lower[dot_pos + 1..];
        if TEXT_EXTENSIONS.contains(&ext) { return true; }
    }
    let base = lower.rsplit('/').next().unwrap_or(&lower);
    matches!(base, "makefile" | "dockerfile" | "readme" | "license" | "changelog" | "authors" | "cargo.lock")
}

/// Same as playground's isBinaryContent
fn is_binary_content(content: &[u8]) -> bool {
    content[..content.len().min(512)].contains(&0)
}

/// Collect files from the rag3db repo, matching the playground's exact filtering.
/// Uses RAG3DB_ROOT env var, or clones from GitHub to /tmp/test_rag3db_clone.
fn collect_repo_files() -> Vec<(String, String)> {
    let root = if let Ok(path) = std::env::var("RAG3DB_ROOT") {
        std::path::PathBuf::from(path)
    } else {
        let clone_path = std::path::Path::new("/tmp/test_rag3db_clone");
        if !clone_path.exists() {
            eprintln!("Cloning https://github.com/L-Defraiteur/rag3db.git ...");
            let status = std::process::Command::new("git")
                .args(&["clone", "--depth", "1", "https://github.com/L-Defraiteur/rag3db.git", clone_path.to_str().unwrap()])
                .status()
                .expect("git clone failed");
            assert!(status.success(), "git clone failed");
        }
        clone_path.to_path_buf()
    };

    eprintln!("Indexing from: {}", root.display());

    // No exclude dirs — the playground indexes everything from the tarball
    // (it doesn't know about .git, build dirs etc. — tarball excludes .git already)
    let exclude_dirs: Vec<&str> = vec![
        ".git",
    ];
    let mut files = Vec::new();

    fn walk(dir: &std::path::Path, exclude: &[&str],
            files: &mut Vec<(String, String)>, root: &std::path::Path) {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            // Skip symlinks (tarball doesn't follow them)
            if path.symlink_metadata().map(|m| m.file_type().is_symlink()).unwrap_or(false) {
                continue;
            }
            if path.is_dir() {
                if !exclude.contains(&name.as_str()) {
                    walk(&path, exclude, files, root);
                }
            } else if path.is_file() {
                let rel = path.strip_prefix(root).unwrap_or(&path).to_string_lossy().to_string();
                // Same extension check as playground
                if !is_text_filename(&rel) { continue; }
                // Same size check
                if let Ok(meta) = path.metadata() {
                    if meta.len() > MAX_FILE_SIZE { continue; }
                }
                let bytes = match std::fs::read(&path) {
                    Ok(b) => b,
                    Err(_) => continue,
                };
                // Same binary check as playground
                if is_binary_content(&bytes) { continue; }
                let content = match String::from_utf8(bytes) {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                if content.is_empty() { continue; }
                files.push((rel, content));
            }
        }
    }

    walk(&root, &exclude_dirs, &mut files, &root);
    files.sort_by(|a, b| a.0.cmp(&b.0));
    files
}


fn paths_of(handle: &LucivyHandle, path_field: ld_lucivy::schema::Field, value: &str, distance: u8)
    -> (std::collections::BTreeSet<String>, usize, usize, usize)
{
    let before = handle.reader.searcher().segment_readers().len();
    let qconfig = QueryConfig {
        query_type: "contains".into(),
        field: Some("content".into()),
        value: Some(value.to_string()),
        distance: Some(distance),
        ..Default::default()
    };
    let results = handle.search(&qconfig, 100_000, None).unwrap();
    let searcher = handle.reader.searcher();
    let after = searcher.segment_readers().len();
    let mut paths = std::collections::BTreeSet::new();
    for (_, addr) in &results {
        let doc: ld_lucivy::LucivyDocument = searcher.doc(*addr).unwrap();
        let path: String = doc.get_first(path_field)
            .map(|v| { let o: ld_lucivy::schema::OwnedValue = v.into(); match o { ld_lucivy::schema::OwnedValue::Str(s) => s, _ => String::new() } })
            .unwrap_or_default();
        paths.insert(path);
    }
    (paths, results.len(), before, after)
}

#[test]
#[ignore]
fn diag_flou_monotonie() {
    let files = collect_repo_files();
    eprintln!("Collected {} files", files.len());
    let tmp_path = std::path::Path::new("/tmp/diag_flou_monotonie");
    let _ = std::fs::remove_dir_all(tmp_path);
    std::fs::create_dir_all(tmp_path).unwrap();
    let config = SchemaConfig {
        fields: vec![
            query::FieldDef { name: "path".into(), field_type: "text".into(),
                stored: Some(true), indexed: Some(false), fast: None },
            query::FieldDef { name: "content".into(), field_type: "text".into(),
                stored: Some(true), indexed: Some(true), fast: None },
        ],
        ..Default::default()
    };
    let dir = StdFsDirectory::open(tmp_path).unwrap();
    let handle = LucivyHandle::create(dir, &config).unwrap();
    let path_field = handle.field("path").unwrap();
    let content_field = handle.field("content").unwrap();
    {
        let mut guard = handle.writer.lock().unwrap();
        let writer = guard.as_mut().unwrap();
        for (i, (path, content)) in files.iter().enumerate() {
            let mut doc = ld_lucivy::LucivyDocument::new();
            doc.add_text(path_field, path);
            doc.add_text(content_field, content);
            writer.add_document(doc).unwrap();
            if (i + 1) % COMMIT_EVERY == 0 { writer.commit().unwrap(); }
        }
        writer.commit().unwrap();
    }
    handle.reader.reload().unwrap();
    // Ground truth by bytes, case-insensitive, like contains d=0.
    let truth: std::collections::BTreeSet<String> = files.iter()
        .filter(|(_, c)| c.to_lowercase().contains("rag3weaver"))
        .map(|(p, _)| p.clone()).collect();
    eprintln!("truth: {} files contain rag3weaver (case-insensitive)", truth.len());
    // The original test's queries, in its order, before its monotonicity check.
    if std::env::var("DIAG_PREAMBLE").is_ok_and(|v| v == "1") {
        let cases: Vec<(&str, u8)> = vec![
            ("rag3weaver", 0), ("rag3weaver", 1), ("rak3weaver", 1), ("rag3db", 1),
            ("Build rag3weaver Rust static lib for WASM emscripten Only used in WASM builds Native", 0),
            ("Build rag3weaver Rust static lib for WASM emscripten Only used in WASM builds Native", 1),
        ];
        for (q, d) in cases {
            let sink = Arc::new(ld_lucivy::query::HighlightSink::new());
            let qconfig = QueryConfig {
                query_type: "contains".into(), field: Some("content".into()),
                value: Some(q.to_string()), distance: Some(d), ..Default::default()
            };
            let r = handle.search(&qconfig, 20, Some(sink)).unwrap();
            eprintln!("preamble {q:.30} d={d}: {} hits", r.len());
            if std::env::var("DIAG_CHECK_EACH").is_ok_and(|v| v == "1") {
                let (d0, ..) = paths_of(&handle, path_field, "rag3weaver", 0);
                let (d1, ..) = paths_of(&handle, path_field, "rag3weaver", 1);
                eprintln!("  after it: rag3weaver d0 {} d1 {}", d0.len(), d1.len());
            }
        }
    }
    let mut red = 0;
    for round in 0..6 {
        let (d0, n0, b0, a0) = paths_of(&handle, path_field, "rag3weaver", 0);
        let (d1, n1, b1, a1) = paths_of(&handle, path_field, "rag3weaver", 1);
        let missing: Vec<_> = d0.difference(&d1).cloned().collect();
        let d0_vs_truth: Vec<_> = truth.difference(&d0).cloned().collect();
        let d1_vs_truth: Vec<_> = truth.difference(&d1).cloned().collect();
        eprintln!("round {round}: d0 {} paths ({n0} hits, segs {b0}->{a0}) d1 {} paths ({n1} hits, segs {b1}->{a1}) \
            missing d0\\d1 {} | truth\\d0 {} | truth\\d1 {}",
            d0.len(), d1.len(), missing.len(), d0_vs_truth.len(), d1_vs_truth.len());
        for p in missing.iter().chain(d1_vs_truth.iter()).take(12) {
            let content = &files.iter().find(|(q, _)| q == p).unwrap().1;
            let lower = content.to_lowercase();
            let pos: Vec<usize> = lower.match_indices("rag3weaver").map(|(i, _)| i).take(4).collect();
            let ctx: Vec<String> = pos.iter().map(|&i| {
                let s = (0..=i.saturating_sub(12)).rev().find(|&k| content.is_char_boundary(k)).unwrap_or(0);
                let e = (i + 22..=content.len()).find(|&k| content.is_char_boundary(k)).unwrap_or(content.len());
                content[s..e].replace('\n', "\\n")
            }).collect();
            eprintln!("    {p} len={} occurrences={} at {:?} ctx={:?}", content.len(),
                lower.matches("rag3weaver").count(), pos, ctx);
        }
        if !missing.is_empty() || !d1_vs_truth.is_empty() { red += 1; }
    }
    eprintln!("rounds red: {red}/6");
    let _ = content_field;
}


fn check(handle: &LucivyHandle, path_field: ld_lucivy::schema::Field,
         files: &[(String, String)], truth: &std::collections::BTreeSet<String>, label: &str) -> bool {
    let (d0, n0, ..) = paths_of(handle, path_field, "rag3weaver", 0);
    let (d1, n1, ..) = paths_of(handle, path_field, "rag3weaver", 1);
    let segs = handle.reader.searcher().segment_readers().iter()
        .map(|r| r.num_docs().to_string()).collect::<Vec<_>>().join(",");
    let missing: Vec<_> = d0.difference(&d1).cloned().chain(truth.difference(&d1).cloned())
        .collect::<std::collections::BTreeSet<_>>().into_iter().collect();
    eprintln!("{label}: d0 {} ({n0}) d1 {} ({n1}) missing {} | segments [{segs}]", d0.len(), d1.len(), missing.len());
    for p in missing.iter().take(10) {
        let content = &files.iter().find(|(q, _)| q == p).unwrap().1;
        let lower = content.to_lowercase();
        let pos: Vec<usize> = lower.match_indices("rag3weaver").map(|(i, _)| i).take(3).collect();
        let ctx: Vec<String> = pos.iter().map(|&i| {
            let s = (0..=i.saturating_sub(12)).rev().find(|&k| content.is_char_boundary(k)).unwrap_or(0);
            let e = (i + 22..=content.len()).find(|&k| content.is_char_boundary(k)).unwrap_or(content.len());
            content[s..e].replace('\n', "\\n")
        }).collect();
        eprintln!("    {p} len={} occ={} at {:?} ctx={:?}", content.len(), lower.matches("rag3weaver").count(), pos, ctx);
    }
    missing.is_empty()
}

#[test]
#[ignore]
fn diag_flou_formes_de_segments() {
    let files = collect_repo_files();
    let tmp_path = std::path::Path::new("/tmp/diag_flou_formes");
    let _ = std::fs::remove_dir_all(tmp_path);
    std::fs::create_dir_all(tmp_path).unwrap();
    let config = SchemaConfig {
        fields: vec![
            query::FieldDef { name: "path".into(), field_type: "text".into(),
                stored: Some(true), indexed: Some(false), fast: None },
            query::FieldDef { name: "content".into(), field_type: "text".into(),
                stored: Some(true), indexed: Some(true), fast: None },
        ],
        ..Default::default()
    };
    let dir = StdFsDirectory::open(tmp_path).unwrap();
    let handle = LucivyHandle::create(dir, &config).unwrap();
    let path_field = handle.field("path").unwrap();
    let content_field = handle.field("content").unwrap();
    {
        let mut guard = handle.writer.lock().unwrap();
        let writer = guard.as_mut().unwrap();
        // No background merge: the layouts below are made by hand.
        writer.set_merge_policy(Box::new(ld_lucivy::indexer::NoMergePolicy));
        for (i, (path, content)) in files.iter().enumerate() {
            let mut doc = ld_lucivy::LucivyDocument::new();
            doc.add_text(path_field, path);
            doc.add_text(content_field, content);
            writer.add_document(doc).unwrap();
            if (i + 1) % COMMIT_EVERY == 0 { writer.commit().unwrap(); }
        }
        writer.commit().unwrap();
    }
    handle.reader.reload().unwrap();
    let truth: std::collections::BTreeSet<String> = files.iter()
        .filter(|(_, c)| c.to_lowercase().contains("rag3weaver"))
        .map(|(p, _)| p.clone()).collect();
    let mut red = 0;
    if !check(&handle, path_field, &files, &truth, "sans fusion") { red += 1; }
    for k in 2..=6usize {
        let ids = handle.index.searchable_segment_ids().unwrap();
        let groups: Vec<Vec<_>> = ids.chunks(k).filter(|c| c.len() > 1).map(|c| c.to_vec()).collect();
        {
            let mut guard = handle.writer.lock().unwrap();
            let writer = guard.as_mut().unwrap();
            writer.merge_many(&groups).unwrap();
            writer.commit().unwrap();
        }
        handle.reader.reload().unwrap();
        handle.wait_dictionary_fold();
        if !check(&handle, path_field, &files, &truth, &format!("fusion par {k}")) { red += 1; }
    }
    {
        let ids = handle.index.searchable_segment_ids().unwrap();
        let mut guard = handle.writer.lock().unwrap();
        let writer = guard.as_mut().unwrap();
        writer.merge(&ids).unwrap();
        writer.commit().unwrap();
    }
    handle.reader.reload().unwrap();
    handle.wait_dictionary_fold();
    if !check(&handle, path_field, &files, &truth, "un seul segment") { red += 1; }
    eprintln!("formes rouges : {red}");
    let _ = content_field;
}


/// Reopen a saved index (DIAG_INDEX, a copy: opening it may write) and compare
/// d=0 and d=1 by path, with the segment layout.
#[test]
#[ignore]
fn diag_flou_index_sauve() {
    let path = std::env::var("DIAG_INDEX").expect("DIAG_INDEX");
    let dir = StdFsDirectory::open(std::path::Path::new(&path)).unwrap();
    let handle = LucivyHandle::open(dir).unwrap();
    let path_field = handle.field("path").unwrap();
    let layout = || handle.reader.searcher().segment_readers().iter()
        .map(|r| format!("{}:{}", r.segment_id().short_uuid_string(), r.num_docs())).collect::<Vec<_>>().join(",");
    eprintln!("layout [{}]", layout());
    for round in 0..3 {
        let (d0, n0, ..) = paths_of(&handle, path_field, "rag3weaver", 0);
        let (d1, n1, ..) = paths_of(&handle, path_field, "rag3weaver", 1);
        let missing: Vec<_> = d0.difference(&d1).collect();
        eprintln!("round {round}: d0 {} ({n0}) d1 {} ({n1}) missing {} {:?} layout [{}]",
            d0.len(), d1.len(), missing.len(), missing.iter().take(8).collect::<Vec<_>>(), layout());
    }
    // Per segment: which segment holds each missing doc, and its doc ids.
    let searcher = handle.reader.searcher();
    for (ord, reader) in searcher.segment_readers().iter().enumerate() {
        let store = reader.get_store_reader(1).unwrap();
        let mut names = Vec::new();
        for doc_id in 0..reader.max_doc() {
            let doc: ld_lucivy::LucivyDocument = store.get(doc_id).unwrap();
            let p: String = doc.get_first(path_field).map(|v| { let o: ld_lucivy::schema::OwnedValue = v.into(); match o { ld_lucivy::schema::OwnedValue::Str(s) => s, _ => String::new() } }).unwrap_or_default();
            if p.starts_with("tools/build-images") || p.starts_with("tools/wasm/test/browser/rag3weaver") {
                names.push(format!("{doc_id}={p}"));
            }
        }
        if !names.is_empty() {
            eprintln!("segment {ord} {} ({} docs): {:?}", reader.segment_id().short_uuid_string(), reader.max_doc(), names);
        }
    }
}


/// On a saved index (DIAG_INDEX, a copy), which queries find a missing doc.
#[test]
#[ignore]
fn diag_flou_morceaux() {
    let path = std::env::var("DIAG_INDEX").expect("DIAG_INDEX");
    let dir = StdFsDirectory::open(std::path::Path::new(&path)).unwrap();
    let handle = LucivyHandle::open(dir).unwrap();
    let path_field = handle.field("path").unwrap();
    let watched = ["tools/build-images/README.md", "tools/build-images/build.sh",
        "tools/wasm/test/browser/rag3weaver.spec.js", "extension/rag3weaver/Cargo.toml", "README.md"];
    let queries = ["rag3weaver", "rag3weave", "ag3weaver", "rag3wea", "3weaver", "rag3w", "eaver", "weaver",
        "rag3", "rag3xeaver", "rag3weavr", "rag3weaverr", "xrag3weaver", "rag3weaver-backend", "paquet", "binaire", "docker"];
    for q in queries {
        for d in [0u8, 1, 2] {
            let (paths, n, ..) = paths_of(&handle, path_field, q, d);
            let marks: String = watched.iter().map(|w| if paths.contains(*w) { 'X' } else { '.' }).collect();
            eprintln!("{q:>20} d={d}: {n:>5} hits  [{marks}]");
        }
    }
}


/// On a saved index (DIAG_INDEX, a copy): global ids the segment's gmap names
/// that the shard dictionary has no text or META for.
#[test]
#[ignore]
fn diag_flou_ids_sans_texte() {
    let path = std::env::var("DIAG_INDEX").expect("DIAG_INDEX");
    let dir = StdFsDirectory::open(std::path::Path::new(&path)).unwrap();
    let handle = LucivyHandle::open(dir).unwrap();
    let content = handle.field("content").unwrap();
    let searcher = handle.reader.searcher();
    for reader in searcher.segment_readers() {
        let seg = reader.segment_id().uuid_string();
        let gmap_path = std::path::Path::new(&path).join(format!("{seg}.{}.gmap", content.field_id()));
        let bytes = std::fs::read(&gmap_path).unwrap();
        let gmap = ld_lucivy::suffix_fst::gmap::GmapReader::open(&bytes).unwrap();
        let dict = reader.sfx_dictionary_field(content).expect("dictionary field");
        let texts = dict.termtexts_reader().expect("dictionary termtexts");
        let mut missing: Vec<u32> = Vec::new();
        let mut total = 0u32;
        for g in gmap.iter() {
            total += 1;
            if texts.meta(g).is_none() || texts.text(g).is_none() { missing.push(g); }
        }
        let mut ranges: Vec<(u32, u32)> = Vec::new();
        for &g in &missing {
            match ranges.last_mut() { Some(r) if r.1 + 1 == g => r.1 = g, _ => ranges.push((g, g)) }
        }
        eprintln!("segment {seg}: gmap {} ids ({} counted), dictionary {} terms, without text/meta: {}",
            gmap.len(), total, texts.num_terms(), missing.len());
        eprintln!("  ranges (first 20 of {}): {:?}", ranges.len(), ranges.iter().take(20).collect::<Vec<_>>());
        if let (Some(min), Some(max)) = (gmap.iter().min(), gmap.iter().max()) { eprintln!("  gmap ids span {min}..={max}"); }
    }
}


/// Sonde des .fts de rag3weaver (13 novembre 2026, session hotfixs de rag3db, pour le ticket
/// 2026-11-13-un-index-lucivy-ecrit-avant-4-3-5-pendant-des-fusions-garde-ses-mots-perdus) :
/// DIAG_FTS=<copie d'un dossier .fts> parcourt chaque table (Lucivy_*), chaque shard (shard_*),
/// chaque champ texte qui a un .gmap, chaque segment, et compte les ids globaux du gmap que le
/// dictionnaire du shard ne connaît pas (sans texte ou sans META). Ouvre chaque shard avec un
/// writer : à jouer sur une copie neuve, jamais sur un index en service.
#[test]
#[ignore]
fn diag_ids_sans_texte_tous_champs() {
    let root = std::path::PathBuf::from(std::env::var("DIAG_FTS").expect("DIAG_FTS"));
    let mut tables: Vec<_> = std::fs::read_dir(&root).unwrap().filter_map(|e| e.ok()).map(|e| e.path())
        .filter(|p| p.is_dir()).collect();
    tables.sort();
    let (mut grand_ids, mut grand_missing, mut pending_total) = (0u64, 0u64, 0usize);
    for table in tables {
        let mut shards: Vec<_> = std::fs::read_dir(&table).unwrap().filter_map(|e| e.ok()).map(|e| e.path())
            .filter(|p| p.is_dir() && p.file_name().unwrap().to_string_lossy().starts_with("shard_")).collect();
        shards.sort();
        for shard in shards {
            let meta: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(shard.join("meta.json")).unwrap()).unwrap();
            let pending = meta.get("pending_segments").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
            pending_total += pending;
            let dir = StdFsDirectory::open(&shard).unwrap();
            let handle = LucivyHandle::open(dir).unwrap();
            let searcher = handle.reader.searcher();
            for (field, entry) in handle.schema.fields() {
                if !matches!(entry.field_type(), ld_lucivy::schema::FieldType::Str(_)) { continue; }
                let (mut ids, mut missing, mut segs) = (0u64, 0u64, 0usize);
                for reader in searcher.segment_readers() {
                    let seg = reader.segment_id().uuid_string();
                    let gmap_path = shard.join(format!("{seg}.{}.gmap", field.field_id()));
                    let Ok(bytes) = std::fs::read(&gmap_path) else { continue };
                    let gmap = ld_lucivy::suffix_fst::gmap::GmapReader::open(&bytes).unwrap();
                    let Some(dict) = reader.sfx_dictionary_field(field) else {
                        eprintln!("{}/{} {}: segment {seg} sans dictionnaire", table.display(), shard.display(), entry.name());
                        continue;
                    };
                    let texts = dict.termtexts_reader().expect("dictionary termtexts");
                    segs += 1;
                    for g in gmap.iter() {
                        ids += 1;
                        if texts.meta(g).is_none() || texts.text(g).is_none() { missing += 1; }
                    }
                }
                if segs > 0 {
                    eprintln!("CHAMP {} {} {} : segments {segs}, ids {ids}, inconnus {missing}{}",
                        table.file_name().unwrap().to_string_lossy(), shard.file_name().unwrap().to_string_lossy(),
                        entry.name(), if missing > 0 { "  <== PERDUS" } else { "" });
                }
                grand_ids += ids;
                grand_missing += missing;
            }
            if pending > 0 {
                eprintln!("ATTENTE {} : {pending} segments dans pending_segments", shard.display());
            }
        }
    }
    eprintln!("BILAN {} : ids {grand_ids}, inconnus du dictionnaire {grand_missing}, segments en attente {pending_total}",
        root.display());
}
