//! Stress test: validate threading refactoring (flume channels, Mutex/Condvar reply, commit).
//!
//! Not a micro-benchmark — this tests correctness and stability under concurrent load.
//! Runs index/commit/search cycles at various thread counts, verifying no deadlocks,
//! no corruption, and document counts stay consistent.
//!
//! Usage:
//!   cargo bench --bench stress_threading
//!
//! This uses criterion for measurement but the primary goal is stability validation.

use criterion::{criterion_group, criterion_main, Criterion};
use ld_lucivy::collector::{Count, TopDocs};
use ld_lucivy::query::{AutomatonPhraseQuery, FuzzyParams, NgramContainsQuery, VerificationMode};
use ld_lucivy::schema::{
    IndexRecordOption, Schema, TextFieldIndexing, TextOptions, STORED,
};
use ld_lucivy::tokenizer::{LowerCaser, NgramTokenizer, TextAnalyzer};
use ld_lucivy::{doc, Index, IndexWriter, ReloadPolicy};
use std::path::Path;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Dataset
// ---------------------------------------------------------------------------

struct SourceFile {
    path: String,
    content: String,
    extension: String,
}

const EXTENSIONS: &[&str] = &[".rs", ".md", ".toml", ".js", ".ts", ".py", ".json"];
const SKIP_DIRS: &[&str] = &[
    "target", "node_modules", "__pycache__", ".venv", "pkg", ".git", "playground",
];
const MAX_FILE_SIZE: u64 = 100_000;

fn collect_source_files() -> Vec<SourceFile> {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    collect_recursive(repo_root, repo_root, &mut files);
    files.sort_by(|a, b| a.path.cmp(&b.path));
    files
}

fn collect_recursive(root: &Path, dir: &Path, files: &mut Vec<SourceFile>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            if SKIP_DIRS.iter().any(|s| *s == name.as_ref()) {
                continue;
            }
            collect_recursive(root, &path, files);
        } else if path.is_file() {
            let ext = path
                .extension()
                .map(|e| format!(".{}", e.to_string_lossy()))
                .unwrap_or_default();
            if !EXTENSIONS.iter().any(|e| *e == ext) {
                continue;
            }
            if let Ok(meta) = path.metadata() {
                if meta.len() > MAX_FILE_SIZE {
                    continue;
                }
            }
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            if name == "package-lock.json" {
                continue;
            }
            if let Ok(content) = std::fs::read_to_string(&path) {
                let rel = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .to_string();
                files.push(SourceFile {
                    path: rel,
                    content,
                    extension: ext,
                });
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

struct Fields {
    path: ld_lucivy::schema::Field,
    content: ld_lucivy::schema::Field,
    content_raw: ld_lucivy::schema::Field,
    content_ngram: ld_lucivy::schema::Field,
    extension: ld_lucivy::schema::Field,
}

fn build_schema() -> (Schema, Fields) {
    let mut builder = Schema::builder();
    let path = builder.add_text_field("path", STORED);
    let content_opts = TextOptions::default()
        .set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer("default")
                .set_index_option(IndexRecordOption::WithFreqsAndPositions),
        )
        .set_stored();
    let content = builder.add_text_field("content", content_opts);
    let raw_opts = TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer("default")
            .set_index_option(IndexRecordOption::WithFreqsAndPositionsAndOffsets),
    );
    let content_raw = builder.add_text_field("content._raw", raw_opts);
    let ngram_opts = TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer("ngram3")
            .set_index_option(IndexRecordOption::Basic),
    );
    let content_ngram = builder.add_text_field("content._ngram", ngram_opts);
    let extension = builder.add_text_field("extension", STORED);
    let schema = builder.build();
    let fields = Fields {
        path,
        content,
        content_raw,
        content_ngram,
        extension,
    };
    (schema, fields)
}

fn create_index(schema: &Schema) -> Index {
    let index = Index::create_in_ram(schema.clone());
    index.tokenizers().register(
        "ngram3",
        TextAnalyzer::builder(NgramTokenizer::new(2, 3, false).unwrap())
            .filter(LowerCaser)
            .build(),
    );
    index
}

// ---------------------------------------------------------------------------
// Stress tests
// ---------------------------------------------------------------------------

/// Stress: repeated index/commit cycles at high thread count.
/// Validates flume channels + Mutex/Condvar reply don't deadlock or corrupt.
fn bench_stress_commit_cycles(c: &mut Criterion) {
    let files = collect_source_files();
    let (schema, fields) = build_schema();
    let num_files = files.len();

    eprintln!("Stress test dataset: {} files", num_files);

    let mut group = c.benchmark_group("stress_threading");
    group.sample_size(10);

    for num_threads in [1, 2, 4, 8] {
        group.bench_function(format!("commit_cycles_{num_threads}t"), |b| {
            b.iter(|| {
                let index = create_index(&schema);
                let heap = if num_threads <= 2 { 50_000_000 } else { num_threads * 20_000_000 };
                let mut writer: IndexWriter =
                    index.writer_with_num_threads(num_threads, heap).unwrap();
                writer.set_merge_policy(Box::new(ld_lucivy::merge_policy::NoMergePolicy));

                // 5 cycles of add-half + commit
                let chunk_size = num_files / 5;
                for cycle in 0..5 {
                    let start = cycle * chunk_size;
                    let end = if cycle == 4 { num_files } else { start + chunk_size };
                    for file in &files[start..end] {
                        writer
                            .add_document(doc!(
                                fields.path => file.path.as_str(),
                                fields.content => file.content.as_str(),
                                fields.content_raw => file.content.as_str(),
                                fields.content_ngram => file.content.as_str(),
                                fields.extension => file.extension.as_str(),
                            ))
                            .unwrap();
                    }
                    writer.commit().unwrap();
                }

                // Verify total doc count
                let reader = index
                    .reader_builder()
                    .reload_policy(ReloadPolicy::Manual)
                    .try_into()
                    .unwrap();
                let searcher = reader.searcher();
                let total = searcher.num_docs() as usize;
                assert_eq!(
                    total, num_files,
                    "Doc count mismatch after 5 commit cycles: expected {num_files}, got {total}"
                );

                writer.wait_merging_threads().unwrap();
            })
        });
    }

    group.finish();
}

/// Stress: concurrent index + search.
/// Writer indexes while readers search — validates no deadlock between
/// writer channels and reader access.
fn bench_stress_concurrent_index_search(c: &mut Criterion) {
    let files = collect_source_files();
    let (schema, fields) = build_schema();
    let num_files = files.len();

    let mut group = c.benchmark_group("stress_concurrent");
    group.sample_size(10);

    for num_threads in [2, 4] {
        group.bench_function(format!("index_while_search_{num_threads}t"), |b| {
            b.iter(|| {
                let index = create_index(&schema);
                let heap = 100_000_000;
                let mut writer: IndexWriter =
                    index.writer_with_num_threads(num_threads, heap).unwrap();
                writer.set_merge_policy(Box::new(ld_lucivy::merge_policy::NoMergePolicy));

                // Index first half and commit (so readers have something to search)
                let mid = num_files / 2;
                for file in &files[..mid] {
                    writer
                        .add_document(doc!(
                            fields.path => file.path.as_str(),
                            fields.content => file.content.as_str(),
                            fields.content_raw => file.content.as_str(),
                            fields.content_ngram => file.content.as_str(),
                            fields.extension => file.extension.as_str(),
                        ))
                        .unwrap();
                }
                writer.commit().unwrap();

                // Spawn search threads that hammer the reader while we index the second half
                let reader = Arc::new(
                    index
                        .reader_builder()
                        .reload_policy(ReloadPolicy::Manual)
                        .try_into()
                        .unwrap(),
                );

                let search_done = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let mut search_handles = Vec::new();

                for _ in 0..2 {
                    let reader = Arc::clone(&reader);
                    let done = Arc::clone(&search_done);
                    search_handles.push(std::thread::spawn(move || {
                        let mut iterations = 0u64;
                        while !done.load(std::sync::atomic::Ordering::Relaxed) {
                            let searcher = reader.searcher();
                            // Simple term search
                            let query = NgramContainsQuery::new(
                                ld_lucivy::schema::Field::from_field_id(2), // content._raw
                                ld_lucivy::schema::Field::from_field_id(3), // content._ngram
                                Some(ld_lucivy::schema::Field::from_field_id(1)), // content
                                vec!["fn".to_string()],
                                VerificationMode::Fuzzy(FuzzyParams {
                                    tokens: vec!["fn".to_string()],
                                    separators: vec![],
                                    prefix: String::new(),
                                    suffix: String::new(),
                                    fuzzy_distance: 0,
                                    distance_budget: 0,
                                    strict_separators: false,
                                }),
                            );
                            let _ = searcher.search(&query, &Count).unwrap();
                            iterations += 1;
                        }
                        iterations
                    }));
                }

                // Index second half while searches are running
                for file in &files[mid..] {
                    writer
                        .add_document(doc!(
                            fields.path => file.path.as_str(),
                            fields.content => file.content.as_str(),
                            fields.content_raw => file.content.as_str(),
                            fields.content_ngram => file.content.as_str(),
                            fields.extension => file.extension.as_str(),
                        ))
                        .unwrap();
                }
                writer.commit().unwrap();

                // Stop search threads
                search_done.store(true, std::sync::atomic::Ordering::Relaxed);
                let total_search_iters: u64 = search_handles
                    .into_iter()
                    .map(|h| h.join().unwrap())
                    .sum();
                eprintln!("  search iterations during indexing: {total_search_iters}");

                writer.wait_merging_threads().unwrap();
            })
        });
    }

    group.finish();
}

/// Bench: startsWith vs contains on same terms.
fn bench_starts_with_vs_contains(c: &mut Criterion) {
    let files = collect_source_files();
    let (schema, fields) = build_schema();

    let index = create_index(&schema);
    {
        let mut writer: IndexWriter =
            index.writer_with_num_threads(2, 50_000_000).unwrap();
        writer.set_merge_policy(Box::new(ld_lucivy::merge_policy::NoMergePolicy));
        for file in &files {
            writer
                .add_document(doc!(
                    fields.path => file.path.as_str(),
                    fields.content => file.content.as_str(),
                    fields.content_raw => file.content.as_str(),
                    fields.content_ngram => file.content.as_str(),
                    fields.extension => file.extension.as_str(),
                ))
                .unwrap();
        }
        writer.commit().unwrap();
        writer.wait_merging_threads().unwrap();
    }

    let reader = index
        .reader_builder()
        .reload_policy(ReloadPolicy::Manual)
        .try_into()
        .unwrap();
    let searcher = reader.searcher();

    // Queries: (label, search_term, is_prefix_friendly)
    let queries = vec![
        ("fn", "fn"),
        ("handle_b", "handle_b"),
        ("segment", "segment"),
        ("auto", "auto"),
        ("query_p", "query_p"),
    ];

    let mut group = c.benchmark_group("startswith_vs_contains");

    for (label, term) in &queries {
        // contains (ngram path)
        group.bench_function(format!("contains_{label}"), |b| {
            b.iter(|| {
                let query = NgramContainsQuery::new(
                    fields.content_raw,
                    fields.content_ngram,
                    Some(fields.content),
                    vec![term.to_string()],
                    VerificationMode::Fuzzy(FuzzyParams {
                        tokens: vec![term.to_string()],
                        separators: vec![],
                        prefix: String::new(),
                        suffix: String::new(),
                        fuzzy_distance: 0,
                        distance_budget: 0,
                        strict_separators: false,
                    }),
                );
                let top_docs = searcher
                    .search(&query, &TopDocs::with_limit(20).order_by_score())
                    .unwrap();
                std::hint::black_box(top_docs);
            })
        });

        // startsWith (FST path) — single token prefix
        group.bench_function(format!("startswith_{label}"), |b| {
            b.iter(|| {
                let query = AutomatonPhraseQuery::new_starts_with(
                    fields.content_raw,
                    vec![(0, term.to_string())],
                    50,
                    0,
                );
                let top_docs = searcher
                    .search(&query, &TopDocs::with_limit(20).order_by_score())
                    .unwrap();
                std::hint::black_box(top_docs);
            })
        });
    }

    // Multi-token startsWith
    group.bench_function("startswith_multi_fn_new", |b| {
        b.iter(|| {
            let query = AutomatonPhraseQuery::new_starts_with(
                fields.content_raw,
                vec![(0, "fn".to_string()), (1, "new".to_string())],
                50,
                0,
            );
            let top_docs = searcher
                .search(&query, &TopDocs::with_limit(20).order_by_score())
                .unwrap();
            std::hint::black_box(top_docs);
        })
    });

    // Multi-token contains for comparison
    group.bench_function("contains_multi_fn_new", |b| {
        b.iter(|| {
            let query = NgramContainsQuery::new(
                fields.content_raw,
                fields.content_ngram,
                Some(fields.content),
                vec!["fn".to_string(), "new".to_string()],
                VerificationMode::Fuzzy(FuzzyParams {
                    tokens: vec!["fn".to_string(), "new".to_string()],
                    separators: vec![" ".to_string()],
                    prefix: String::new(),
                    suffix: String::new(),
                    fuzzy_distance: 0,
                    distance_budget: 0,
                    strict_separators: false,
                }),
            );
            let top_docs = searcher
                .search(&query, &TopDocs::with_limit(20).order_by_score())
                .unwrap();
            std::hint::black_box(top_docs);
        })
    });

    group.finish();
}

/// Stress: 8 threads, rapid-fire add+commit in tight loop.
/// This is the most aggressive test — validates the flume channel
/// doesn't lose messages and commit doesn't deadlock under pressure.
fn bench_stress_rapid_commits(c: &mut Criterion) {
    let files = collect_source_files();
    let (schema, fields) = build_schema();

    let mut group = c.benchmark_group("stress_rapid_commits");
    group.sample_size(10);

    group.bench_function("50_commits_8threads", |b| {
        b.iter(|| {
            let index = create_index(&schema);
            let mut writer: IndexWriter =
                index.writer_with_num_threads(8, 200_000_000).unwrap();
            writer.set_merge_policy(Box::new(ld_lucivy::merge_policy::NoMergePolicy));

            let docs_per_commit = files.len().max(1) / 50;
            let mut doc_idx = 0;
            let mut total_added = 0;

            for _ in 0..50 {
                for _ in 0..docs_per_commit {
                    let file = &files[doc_idx % files.len()];
                    writer
                        .add_document(doc!(
                            fields.path => file.path.as_str(),
                            fields.content => file.content.as_str(),
                            fields.content_raw => file.content.as_str(),
                            fields.content_ngram => file.content.as_str(),
                            fields.extension => file.extension.as_str(),
                        ))
                        .unwrap();
                    doc_idx += 1;
                    total_added += 1;
                }
                writer.commit().unwrap();
            }

            // Verify integrity
            let reader = index
                .reader_builder()
                .reload_policy(ReloadPolicy::Manual)
                .try_into()
                .unwrap();
            let searcher = reader.searcher();
            let count = searcher.num_docs() as usize;
            assert_eq!(
                count, total_added,
                "Expected {total_added} docs after 50 commits, got {count}"
            );

            writer.wait_merging_threads().unwrap();
        })
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_stress_commit_cycles,
    bench_stress_concurrent_index_search,
    bench_starts_with_vs_contains,
    bench_stress_rapid_commits,
);
criterion_main!(benches);
