//! Benchmark NGram indexing + contains search on real source code.
//!
//! Reproduces the playground workload: index lucivy source files with
//! triple-field layout (base, _raw, _ngram), then search with
//! NgramContainsQuery.
//!
//! Usage:
//!   cargo bench --bench ngram_bench
//!
//! To compare branches:
//!   git checkout main
//!   cargo bench --bench ngram_bench -- --save-baseline main
//!   git checkout scheduler-beta
//!   cargo bench --bench ngram_bench -- --save-baseline scheduler-beta
//!   critcmp main scheduler-beta

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use ld_lucivy::query::{FuzzyParams, NgramContainsQuery, VerificationMode};
use ld_lucivy::schema::{
    IndexRecordOption, Schema, TextFieldIndexing, TextOptions, STORED,
};
use ld_lucivy::tokenizer::{
    LowerCaser, NgramTokenizer, TextAnalyzer,
};
use ld_lucivy::{doc, Index, IndexWriter, ReloadPolicy};
use std::path::Path;

// ---------------------------------------------------------------------------
// Dataset: collect source files from the repo
// ---------------------------------------------------------------------------

struct SourceFile {
    path: String,
    content: String,
    extension: String,
}

const EXTENSIONS: &[&str] = &[".rs", ".md", ".toml", ".js", ".ts", ".py", ".json"];
const SKIP_DIRS: &[&str] = &[
    "target",
    "node_modules",
    "__pycache__",
    ".venv",
    "pkg",
    ".git",
    "playground",
];
const MAX_FILE_SIZE: u64 = 100_000;

fn collect_source_files() -> Vec<SourceFile> {
    let repo_root =
        Path::new(env!("CARGO_MANIFEST_DIR"));
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
// Schema: triple-field layout like lucivy_core
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

    // path — stored, default tokenizer
    let path = builder.add_text_field("path", STORED);

    // content — stored, default tokenizer
    let content_opts = TextOptions::default()
        .set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer("default")
                .set_index_option(IndexRecordOption::WithFreqsAndPositions),
        )
        .set_stored();
    let content = builder.add_text_field("content", content_opts);

    // content._raw — lowercase only, positions + offsets
    let raw_opts = TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer("default")
            .set_index_option(IndexRecordOption::WithFreqsAndPositionsAndOffsets),
    );
    let content_raw = builder.add_text_field("content._raw", raw_opts);

    // content._ngram — trigrams, basic (doc IDs only)
    let ngram_opts = TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer("ngram3")
            .set_index_option(IndexRecordOption::Basic),
    );
    let content_ngram = builder.add_text_field("content._ngram", ngram_opts);

    // extension — stored
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
    // Register the ngram tokenizer (trigrams 2-3, non-prefix)
    index.tokenizers().register(
        "ngram3",
        TextAnalyzer::builder(NgramTokenizer::new(2, 3, false).unwrap())
            .filter(LowerCaser)
            .build(),
    );
    index
}

// ---------------------------------------------------------------------------
// Benchmarks
// ---------------------------------------------------------------------------

fn bench_index_and_commit(c: &mut Criterion) {
    let files = collect_source_files();
    let total_bytes: usize = files.iter().map(|f| f.content.len()).sum();
    let num_files = files.len();

    eprintln!(
        "Dataset: {} files, {:.1} MB",
        num_files,
        total_bytes as f64 / 1_048_576.0
    );

    let (schema, fields) = build_schema();

    let mut group = c.benchmark_group("ngram_indexing");
    group.throughput(Throughput::Bytes(total_bytes as u64));
    group.sample_size(10);

    // 1 thread
    group.bench_function("index_commit_1thread", |b| {
        b.iter(|| {
            let index = create_index(&schema);
            let mut writer: IndexWriter =
                index.writer_with_num_threads(1, 50_000_000).unwrap();
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
        })
    });

    // 2 threads
    group.bench_function("index_commit_2threads", |b| {
        b.iter(|| {
            let index = create_index(&schema);
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
        })
    });

    // 4 threads
    group.bench_function("index_commit_4threads", |b| {
        b.iter(|| {
            let index = create_index(&schema);
            let mut writer: IndexWriter =
                index.writer_with_num_threads(4, 100_000_000).unwrap();
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
        })
    });

    group.finish();
}

fn bench_contains_search(c: &mut Criterion) {
    let files = collect_source_files();
    let (schema, fields) = build_schema();

    // Build the index once for search benchmarks
    let index = create_index(&schema);
    {
        let mut writer: IndexWriter =
            index.writer_with_num_threads(1, 50_000_000).unwrap();
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

    let queries: Vec<(&str, Vec<String>)> = vec![
        // Short token — many trigram hits
        ("fn ", vec!["fn".to_string()]),
        // Medium — typical code search
        ("handle_batch", vec!["handle_batch".to_string()]),
        // Multi-word contains_split style
        ("actor priority", vec!["actor".to_string(), "priority".to_string()]),
        // Long substring
        ("SegmentUpdaterMsg", vec!["segmentupdatermsg".to_string()]),
        // Fuzzy (distance 1)
        ("schedulr", vec!["schedulr".to_string()]),
    ];

    let mut group = c.benchmark_group("ngram_search");

    for (label, tokens) in &queries {
        group.bench_function(format!("contains_{}", label.replace(' ', "_")), |b| {
            b.iter(|| {
                let query = NgramContainsQuery::new(
                    fields.content_raw,
                    fields.content_ngram,
                    Some(fields.content),
                    tokens.clone(),
                    VerificationMode::Fuzzy(FuzzyParams {
                        tokens: tokens.clone(),
                        separators: if tokens.len() > 1 {
                            vec![" ".to_string()]
                        } else {
                            vec![]
                        },
                        prefix: String::new(),
                        suffix: String::new(),
                        fuzzy_distance: if *label == "schedulr" { 1 } else { 0 },
                        distance_budget: if *label == "schedulr" { 1 } else { 0 },
                        strict_separators: false,
                    }),
                );
                let top_docs = searcher
                    .search(&query, &ld_lucivy::collector::TopDocs::with_limit(20).order_by_score())
                    .unwrap();
                std::hint::black_box(top_docs);
            })
        });
    }

    group.finish();
}

criterion_group!(benches, bench_index_and_commit, bench_contains_search);
criterion_main!(benches);
