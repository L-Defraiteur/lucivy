//! Head-to-head benchmark: lucivy (1-shard + 4-shard) vs tantivy 0.25.
//!
//! Uses the same dataset (Linux kernel source files) and same queries.
//! Both engines use SimpleTokenizer + LowerCaser for fair comparison.
//!
//! Run with:
//!   cargo test -p lucivy-core --test bench_vs_tantivy -- --nocapture
//!   MAX_DOCS=5000 cargo test -p lucivy-core --test bench_vs_tantivy -- --nocapture

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

// ── Lucivy imports ──
use lucivy_core::handle::{LucivyHandle, NODE_ID_FIELD};
use lucivy_core::query::{self, QueryConfig};
use lucivy_core::sharded_handle::ShardedHandle;

// ── Tantivy imports ──
use tantivy::collector::TopDocs;
use tantivy::query::{PhraseQuery as TvPhraseQuery, QueryParser as TvQueryParser, TermQuery as TvTermQuery};
use tantivy::schema::{Schema as TvSchema, TEXT, STORED, IndexRecordOption as TvIndexRecordOption};
use tantivy::{Index as TvIndex, Term as TvTerm, IndexWriter as TvIndexWriter, doc};

const LINUX_CLONE: &str = "/home/luciedefraiteur/linux_bench";
const BENCH_BASE: &str = "/home/luciedefraiteur/lucivy_bench_vs_tantivy";
// Reuse existing lucivy indexes from the sharding bench
const SHARDING_BENCH_BASE: &str = "/home/luciedefraiteur/lucivy_bench_sharding";
const MAX_FILE_SIZE: u64 = 100_000;

fn remove_locks(dir: &Path) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                remove_locks(&path);
            } else if path.file_name().map_or(false, |n| n.to_string_lossy().ends_with(".lock")) {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
}

// ─── File collection (shared) ────────────────────────────────────────────

fn is_text(path: &Path) -> bool {
    let Ok(data) = std::fs::read(path) else { return false };
    let chunk = &data[..data.len().min(8192)];
    if chunk.contains(&0u8) { return false; }
    std::str::from_utf8(chunk).is_ok()
}

fn collect_files(root: &Path, max_docs: usize) -> Vec<(String, String)> {
    use std::collections::HashSet;
    let exclude_dirs = [
        "target", "node_modules", ".git", "build", "build_wasm", "pkg",
        "__pycache__", ".venv", ".pytest_cache", "playground",
    ];
    let exclude_files = ["package-lock.json", ".env", ".gitignore"];
    let mut files = Vec::new();
    let mut visited = HashSet::new();

    fn walk(
        dir: &Path, root: &Path,
        exclude_dirs: &[&str], exclude_files: &[&str],
        files: &mut Vec<(String, String)>,
        visited: &mut HashSet<std::path::PathBuf>,
        max_docs: usize,
    ) {
        if files.len() >= max_docs { return; }
        let canonical = match dir.canonicalize() { Ok(p) => p, Err(_) => return };
        if !visited.insert(canonical) { return; }
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            if files.len() >= max_docs { return; }
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if path.is_dir() || path.is_symlink() && path.metadata().map(|m| m.is_dir()).unwrap_or(false) {
                if !exclude_dirs.contains(&name.as_str()) {
                    walk(&path, root, exclude_dirs, exclude_files, files, visited, max_docs);
                }
            } else if path.is_file() {
                if exclude_files.contains(&name.as_str()) { continue; }
                let size = path.metadata().map(|m| m.len()).unwrap_or(0);
                if size > MAX_FILE_SIZE || size == 0 { continue; }
                if !is_text(&path) { continue; }
                let rel = path.strip_prefix(root).unwrap_or(&path).to_string_lossy().to_string();
                if let Ok(content) = std::fs::read_to_string(&path) {
                    if !content.trim().is_empty() {
                        files.push((rel, content));
                    }
                }
            }
        }
    }

    walk(root, root, &exclude_dirs, &exclude_files, &mut files, &mut visited, max_docs);
    files
}

// ─── Tantivy index ────────────────────────────────────────────────────────

fn index_tantivy(files: &[(String, String)], dir: &str) -> (TvIndex, f64) {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).unwrap();

    let mut schema_builder = TvSchema::builder();
    let tv_path = schema_builder.add_text_field("path", TEXT | STORED);
    let tv_content = schema_builder.add_text_field("content", TEXT | STORED);
    let schema = schema_builder.build();

    let index = TvIndex::create_in_dir(dir, schema).unwrap();
    let mut writer: TvIndexWriter = index.writer(100_000_000).unwrap();

    let t0 = Instant::now();
    for (i, (path, content)) in files.iter().enumerate() {
        writer.add_document(doc!(
            tv_path => path.as_str(),
            tv_content => content.as_str(),
        )).unwrap();
        if (i + 1) % 5000 == 0 {
            writer.commit().unwrap();
            eprintln!("    tantivy committed {}/{} ({:.1}s)", i + 1, files.len(), t0.elapsed().as_secs_f64());
        }
    }
    writer.commit().unwrap();
    let elapsed = t0.elapsed().as_secs_f64();
    (index, elapsed)
}

// ─── Lucivy indexes ───────────────────────────────────────────────────────

fn index_lucivy_single(files: &[(String, String)], dir: &str) -> (LucivyHandle, f64) {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).unwrap();

    let config: query::SchemaConfig = serde_json::from_value(serde_json::json!({
        "fields": [
            {"name": "path", "type": "text", "stored": true},
            {"name": "content", "type": "text", "stored": true}
        ]
    })).unwrap();

    let d = lucivy_core::directory::StdFsDirectory::open(dir).unwrap();
    let handle = LucivyHandle::create(d, &config).unwrap();
    let path_f = handle.field("path").unwrap();
    let content_f = handle.field("content").unwrap();
    let nid_f = handle.field(NODE_ID_FIELD).unwrap();

    let t0 = Instant::now();
    {
        let mut guard = handle.writer.lock().unwrap();
        let w = guard.as_mut().unwrap();
        for (i, (path, content)) in files.iter().enumerate() {
            let mut doc = ld_lucivy::LucivyDocument::new();
            doc.add_u64(nid_f, i as u64);
            doc.add_text(path_f, path);
            doc.add_text(content_f, content);
            w.add_document(doc).unwrap();
            if (i + 1) % 5000 == 0 {
                w.commit().unwrap();
                eprintln!("    lucivy-1sh committed {}/{} ({:.1}s)", i + 1, files.len(), t0.elapsed().as_secs_f64());
            }
        }
        w.commit().unwrap();
    }
    let elapsed = t0.elapsed().as_secs_f64();
    handle.reader.reload().unwrap();
    (handle, elapsed)
}

fn index_lucivy_sharded(files: &[(String, String)], dir: &str, num_shards: usize) -> (ShardedHandle, f64) {
    let _ = std::fs::remove_dir_all(dir);
    let config: query::SchemaConfig = serde_json::from_value(serde_json::json!({
        "fields": [
            {"name": "path", "type": "text", "stored": true},
            {"name": "content", "type": "text", "stored": true}
        ],
        "shards": num_shards,
        "balance_weight": 1.0
    })).unwrap();

    let handle = ShardedHandle::create(dir, &config).unwrap();
    let path_f = handle.field("path").unwrap();
    let content_f = handle.field("content").unwrap();
    let nid_f = handle.field(NODE_ID_FIELD).unwrap();

    let t0 = Instant::now();
    for (i, (path, content)) in files.iter().enumerate() {
        let mut doc = ld_lucivy::LucivyDocument::new();
        doc.add_u64(nid_f, i as u64);
        doc.add_text(path_f, path);
        doc.add_text(content_f, content);
        handle.add_document(doc, i as u64).unwrap();
        if (i + 1) % 5000 == 0 {
            handle.commit().unwrap();
            eprintln!("    lucivy-4sh committed {}/{} ({:.1}s)", i + 1, files.len(), t0.elapsed().as_secs_f64());
        }
    }
    handle.commit().unwrap();
    let elapsed = t0.elapsed().as_secs_f64();
    (handle, elapsed)
}

// ─── Query timing helpers ─────────────────────────────────────────────────

fn time_tantivy_term(index: &TvIndex, field_name: &str, value: &str) -> (usize, f64) {
    let reader = index.reader().unwrap();
    let searcher = reader.searcher();
    let field = index.schema().get_field(field_name).unwrap();
    let term = TvTerm::from_field_text(field, value);
    let query = TvTermQuery::new(term, TvIndexRecordOption::WithFreqs);
    let t0 = Instant::now();
    let results = searcher.search(&query, &TopDocs::with_limit(20)).unwrap();
    let ms = t0.elapsed().as_secs_f64() * 1000.0;
    (results.len(), ms)
}

fn time_tantivy_phrase(index: &TvIndex, field_name: &str, terms: &[&str]) -> (usize, f64) {
    let reader = index.reader().unwrap();
    let searcher = reader.searcher();
    let field = index.schema().get_field(field_name).unwrap();
    let tv_terms: Vec<TvTerm> = terms.iter()
        .map(|t| TvTerm::from_field_text(field, t))
        .collect();
    let query = TvPhraseQuery::new(tv_terms);
    let t0 = Instant::now();
    let results = searcher.search(&query, &TopDocs::with_limit(20)).unwrap();
    let ms = t0.elapsed().as_secs_f64() * 1000.0;
    (results.len(), ms)
}

fn time_tantivy_parse(index: &TvIndex, field_name: &str, query_str: &str) -> (usize, f64) {
    let reader = index.reader().unwrap();
    let searcher = reader.searcher();
    let field = index.schema().get_field(field_name).unwrap();
    let parser = TvQueryParser::for_index(index, vec![field]);
    let query = parser.parse_query(query_str).unwrap();
    let t0 = Instant::now();
    let results = searcher.search(&*query, &TopDocs::with_limit(20)).unwrap();
    let ms = t0.elapsed().as_secs_f64() * 1000.0;
    (results.len(), ms)
}

fn time_tantivy_fuzzy(index: &TvIndex, field_name: &str, value: &str, distance: u8) -> (usize, f64) {
    use tantivy::query::FuzzyTermQuery as TvFuzzyTermQuery;
    let reader = index.reader().unwrap();
    let searcher = reader.searcher();
    let field = index.schema().get_field(field_name).unwrap();
    let term = TvTerm::from_field_text(field, value);
    let query = TvFuzzyTermQuery::new(term, distance, true);
    let t0 = Instant::now();
    let results = searcher.search(&query, &TopDocs::with_limit(20)).unwrap();
    let ms = t0.elapsed().as_secs_f64() * 1000.0;
    (results.len(), ms)
}

fn time_tantivy_regex(index: &TvIndex, field_name: &str, pattern: &str) -> (usize, f64) {
    use tantivy::query::RegexQuery as TvRegexQuery;
    let reader = index.reader().unwrap();
    let searcher = reader.searcher();
    let field = index.schema().get_field(field_name).unwrap();
    let query = TvRegexQuery::from_pattern(pattern, field).unwrap();
    let t0 = Instant::now();
    let results = searcher.search(&query, &TopDocs::with_limit(20)).unwrap();
    let ms = t0.elapsed().as_secs_f64() * 1000.0;
    (results.len(), ms)
}

fn time_lucivy_single(handle: &LucivyHandle, config: &QueryConfig) -> (usize, f64) {
    // Build query BEFORE the timer — same as tantivy bench does.
    let query = query::build_query(config, &handle.schema, &handle.index, None).unwrap();
    let t0 = Instant::now();
    let searcher = handle.reader.searcher();
    let collector = ld_lucivy::collector::TopDocs::with_limit(20).order_by_score();
    let results = searcher.search(&*query, &collector).unwrap();
    let ms = t0.elapsed().as_secs_f64() * 1000.0;
    (results.len(), ms)
}

fn time_lucivy_sharded(handle: &ShardedHandle, config: &QueryConfig) -> (usize, f64) {
    let t0 = Instant::now();
    let results = handle.search(config, 20, None).unwrap();
    let ms = t0.elapsed().as_secs_f64() * 1000.0;
    (results.len(), ms)
}

// ─── Bench helper: warmup + 3-run avg ─────────────────────────────────────

fn avg3<F: FnMut() -> (usize, f64)>(mut f: F) -> (usize, f64) {
    let _ = f(); // warmup
    let mut total = 0.0;
    let mut hits = 0;
    for _ in 0..3 {
        let (h, ms) = f();
        total += ms;
        hits = h;
    }
    (hits, total / 3.0)
}

// ─── Main bench ───────────────────────────────────────────────────────────

#[test]
fn bench_vs_tantivy() {
    let max_docs: usize = std::env::var("MAX_DOCS")
        .ok().and_then(|v| v.parse().ok())
        .unwrap_or(90_000);

    let dataset = std::env::var("BENCH_DATASET").unwrap_or_else(|_| LINUX_CLONE.to_string());

    let tv_dir = format!("{BENCH_BASE}/tantivy");

    // Reuse existing lucivy indexes from the sharding bench
    let lv1_dir = format!("{SHARDING_BENCH_BASE}/single");
    let lv4_dir = format!("{SHARDING_BENCH_BASE}/round_robin");

    // ── Build tantivy index from same files (once, reuse on subsequent runs) ──
    if !Path::new(&tv_dir).exists() {
        eprintln!("\n=== Collecting files from {} (max {}) ===", dataset, max_docs);
        let files = collect_files(Path::new(&dataset), max_docs);
        let ndocs = files.len();
        eprintln!("Collected {} text files\n", ndocs);
        if ndocs == 0 { eprintln!("No files. Skipping."); return; }

        eprintln!("=== Indexing: tantivy 0.25 ({} docs) ===", ndocs);
        let (_, tv_time) = index_tantivy(&files, &tv_dir);
        eprintln!("  {} docs in {:.2}s\n", ndocs, tv_time);
    } else {
        eprintln!("\n=== Reusing existing tantivy index at {} ===", tv_dir);
    }

    // ── Check lucivy indexes exist ──
    if !Path::new(&lv4_dir).join("_shard_config.json").exists() {
        eprintln!("No lucivy index at {}. Run the sharding bench first:", lv4_dir);
        eprintln!("  BENCH_MODE=\"SINGLE|RR\" MAX_DOCS=90000 cargo test -p lucivy-core --test bench_sharding -- --nocapture");
        return;
    }

    // ── Open all three ──
    remove_locks(Path::new(&lv1_dir));
    remove_locks(Path::new(&lv4_dir));

    let tv_index = TvIndex::open_in_dir(&tv_dir).unwrap();
    let tv_reader = tv_index.reader().unwrap();
    let tv_ndocs = tv_reader.searcher().num_docs();

    // lucivy single-shard may not exist if bench was run with BENCH_MODE=RR only
    let lv_single = if Path::new(&lv1_dir).exists() {
        let d = lucivy_core::directory::StdFsDirectory::open(&lv1_dir).unwrap();
        Some(LucivyHandle::open(d).unwrap())
    } else {
        eprintln!("No lucivy single-shard index. Skipping Lucivy-1 column.");
        None
    };

    let lv_sharded = ShardedHandle::open(&lv4_dir).unwrap();
    let lv4_ndocs = lv_sharded.num_docs();

    let lv1_ndocs = lv_single.as_ref()
        .map(|h| h.reader.searcher().num_docs())
        .unwrap_or(0);
    eprintln!("Tantivy: {} docs | Lucivy-1: {} docs | Lucivy-4: {} docs\n",
        tv_ndocs, lv1_ndocs, lv4_ndocs);

    // ── Queries ──
    eprintln!("{:<30} {:>6} {:>10} {:>10} {:>10}",
        "Query", "Hits", "Tantivy", "Lucivy-1", "Lucivy-4");
    eprintln!("{}", "-".repeat(70));

    // Helper: run lucivy single if available
    let lv1_avg = |config: &QueryConfig| -> (usize, f64) {
        if let Some(ref h) = lv_single {
            avg3(|| time_lucivy_single(h, config))
        } else { (0, 0.0) }
    };
    let lv1_col = |ms: f64| -> String {
        if lv_single.is_some() { format!("{:>8.1}ms", ms) } else { format!("{:>10}", "---") }
    };

    // Term queries
    for term in &["mutex", "lock", "function", "sched", "printk"] {
        let lv_cfg = QueryConfig {
            query_type: "term".into(), field: Some("content".into()),
            value: Some(term.to_string()), ..Default::default()
        };
        let (h_tv, ms_tv) = avg3(|| time_tantivy_term(&tv_index, "content", term));
        let (h_lv1, ms_lv1) = lv1_avg(&lv_cfg);
        let (h_lv4, ms_lv4) = avg3(|| time_lucivy_sharded(&lv_sharded, &lv_cfg));
        let hits = h_tv.max(h_lv1).max(h_lv4);
        eprintln!("term '{:<20}'  {:>6} {:>8.1}ms {} {:>8.1}ms",
            term, hits, ms_tv, lv1_col(ms_lv1), ms_lv4);
    }

    eprintln!();

    // Phrase queries
    let phrases: Vec<(&str, Vec<&str>)> = vec![
        ("mutex lock", vec!["mutex", "lock"]),
        ("struct device", vec!["struct", "device"]),
        ("return error", vec!["return", "error"]),
        ("unsigned long", vec!["unsigned", "long"]),
    ];
    for (label, terms) in &phrases {
        let lv_cfg = QueryConfig {
            query_type: "phrase".into(), field: Some("content".into()),
            terms: Some(terms.iter().map(|t| t.to_string()).collect()),
            ..Default::default()
        };
        let (h_tv, ms_tv) = avg3(|| time_tantivy_phrase(&tv_index, "content", terms));
        let (h_lv1, ms_lv1) = lv1_avg(&lv_cfg);
        let (h_lv4, ms_lv4) = avg3(|| time_lucivy_sharded(&lv_sharded, &lv_cfg));
        let hits = h_tv.max(h_lv1).max(h_lv4);
        eprintln!("phrase '{:<17}'  {:>6} {:>8.1}ms {} {:>8.1}ms",
            label, hits, ms_tv, lv1_col(ms_lv1), ms_lv4);
    }

    eprintln!();

    // Parse queries (boolean)
    let parse_queries = &[
        "mutex AND lock",
        "function OR struct",
        "\"return error\"",
    ];
    for q in parse_queries {
        let lv_cfg = QueryConfig {
            query_type: "parse".into(), field: Some("content".into()),
            value: Some(q.to_string()), ..Default::default()
        };
        let (h_tv, ms_tv) = avg3(|| time_tantivy_parse(&tv_index, "content", q));
        let (h_lv1, ms_lv1) = lv1_avg(&lv_cfg);
        let (h_lv4, ms_lv4) = avg3(|| time_lucivy_sharded(&lv_sharded, &lv_cfg));
        let hits = h_tv.max(h_lv1).max(h_lv4);
        eprintln!("parse '{:<17}'  {:>6} {:>8.1}ms {} {:>8.1}ms",
            q, hits, ms_tv, lv1_col(ms_lv1), ms_lv4);
    }

    eprintln!();

    // Fuzzy queries (Levenshtein on term dict — same behavior both engines)
    let fuzzy_queries: Vec<(&str, &str, u8)> = vec![
        ("schdule", "schdule", 1),
        ("mutex", "mutex", 2),
        ("fuction", "fuction", 1),
        ("prntk", "prntk", 2),
    ];
    for (label, value, distance) in &fuzzy_queries {
        let lv_cfg = QueryConfig {
            query_type: "fuzzy".into(), field: Some("content".into()),
            value: Some(value.to_string()), distance: Some(*distance),
            ..Default::default()
        };
        let (h_tv, ms_tv) = avg3(|| time_tantivy_fuzzy(&tv_index, "content", value, *distance));
        let (h_lv1, ms_lv1) = lv1_avg(&lv_cfg);
        let (h_lv4, ms_lv4) = avg3(|| time_lucivy_sharded(&lv_sharded, &lv_cfg));
        let hits = h_tv.max(h_lv1).max(h_lv4);
        eprintln!("fuzzy '{:<10}' d={}    {:>6} {:>8.1}ms {} {:>8.1}ms",
            label, distance, hits, ms_tv, lv1_col(ms_lv1), ms_lv4);
    }

    eprintln!();

    // Regex queries (on term dict — same behavior both engines)
    let regex_queries: Vec<(&str, &str)> = vec![
        ("mutex.*", "mutex.*"),
        ("sched[a-z]+", "sched[a-z]+"),
        ("print[kf]", "print[kf]"),
    ];
    for (label, pattern) in &regex_queries {
        let lv_cfg = QueryConfig {
            query_type: "regex".into(), field: Some("content".into()),
            pattern: Some(pattern.to_string()), ..Default::default()
        };
        let (h_tv, ms_tv) = avg3(|| time_tantivy_regex(&tv_index, "content", pattern));
        let (h_lv1, ms_lv1) = lv1_avg(&lv_cfg);
        let (h_lv4, ms_lv4) = avg3(|| time_lucivy_sharded(&lv_sharded, &lv_cfg));
        let hits = h_tv.max(h_lv1).max(h_lv4);
        eprintln!("regex '{:<15}'  {:>6} {:>8.1}ms {} {:>8.1}ms",
            label, hits, ms_tv, lv1_col(ms_lv1), ms_lv4);
    }

    eprintln!();

    // ── Lucivy-only: contains/startsWith (no tantivy equivalent) ──
    eprintln!("{:<30} {:>6} {:>10} {:>10} {:>10}",
        "Lucivy-only", "Hits", "---", "Lucivy-1", "Lucivy-4");
    eprintln!("{}", "-".repeat(70));

    let contains_queries: Vec<(&str, QueryConfig)> = vec![
        ("contains 'mutex_lock'", QueryConfig {
            query_type: "contains".into(), field: Some("content".into()),
            value: Some("mutex_lock".into()), ..Default::default()
        }),
        ("contains 'function'", QueryConfig {
            query_type: "contains".into(), field: Some("content".into()),
            value: Some("function".into()), ..Default::default()
        }),
        ("startsWith 'sched'", QueryConfig {
            query_type: "startsWith".into(), field: Some("content".into()),
            value: Some("sched".into()), ..Default::default()
        }),
        ("fuzzy 'schdule' d=1", QueryConfig {
            query_type: "contains".into(), field: Some("content".into()),
            value: Some("schdule".into()), distance: Some(1), ..Default::default()
        }),
        ("phrase_prefix 'mutex loc'", QueryConfig {
            query_type: "phrase_prefix".into(), field: Some("content".into()),
            value: Some("mutex loc".into()), ..Default::default()
        }),
    ];

    for (label, config) in &contains_queries {
        let (h_lv1, ms_lv1) = lv1_avg(config);
        let (h_lv4, ms_lv4) = avg3(|| time_lucivy_sharded(&lv_sharded, config));
        let hits = h_lv1.max(h_lv4);
        eprintln!("{:<30} {:>6} {:>10} {} {:>8.1}ms",
            label, hits, "N/A", lv1_col(ms_lv1), ms_lv4);
    }

    eprintln!("\n=== {} docs ===", lv4_ndocs);
}
