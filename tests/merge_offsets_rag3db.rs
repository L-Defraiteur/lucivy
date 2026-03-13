//! Integration test: index the rag3db repo with batch commits (like the playground),
//! then search with startsWith + highlights to verify offsets survive segment merges.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ld_lucivy::collector::TopDocs;
use ld_lucivy::query::{FuzzyTermQuery, HighlightSink};
use ld_lucivy::schema::{IndexRecordOption, TextFieldIndexing, TextOptions, Schema, Term};
use ld_lucivy::{doc, Index, IndexWriter};

const RAG3DB_PATH: &str = "/tmp/rag3db-test";
const BATCH_SIZE: usize = 200;
const MAX_FILE_SIZE: usize = 100_000;

fn walk_dir(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            if name == ".git" || name == "node_modules" || name == "target" {
                continue;
            }
            walk_dir(&path, out);
        } else if path.is_file() {
            out.push(path);
        }
    }
}

fn collect_text_files(dir: &Path) -> Vec<(String, String)> {
    let mut paths = Vec::new();
    walk_dir(dir, &mut paths);
    let mut files = Vec::new();
    for path in paths {
        if let Ok(content) = fs::read_to_string(&path) {
            if content.len() > MAX_FILE_SIZE {
                continue;
            }
            let rel = path.strip_prefix(dir).unwrap_or(&path);
            files.push((rel.to_string_lossy().to_string(), content));
        }
    }
    files
}

#[test]
fn test_rag3db_startswith_highlights_after_merge() {
    let rag3db = Path::new(RAG3DB_PATH);
    if !rag3db.exists() {
        eprintln!("Skipping: {} not found (run: git clone --depth 1 https://github.com/L-Defraiteur/rag3db /tmp/rag3db-test)", RAG3DB_PATH);
        return;
    }

    let files = collect_text_files(rag3db);
    eprintln!("Collected {} text files (<={}KB)", files.len(), MAX_FILE_SIZE / 1000);
    assert!(!files.is_empty(), "no files found");

    // Build schema with offsets (same as LucivyHandle)
    let mut schema_builder = Schema::builder();
    let indexing = TextFieldIndexing::default()
        .set_tokenizer("default")
        .set_index_option(IndexRecordOption::WithFreqsAndPositionsAndOffsets);
    let text_opts = TextOptions::default()
        .set_indexing_options(indexing)
        .set_stored();
    let path_field = schema_builder.add_text_field("path", text_opts.clone());
    let content_field = schema_builder.add_text_field("content", text_opts);
    let schema = schema_builder.build();

    let index = Index::create_in_ram(schema);

    // Index with batch commits (like playground: commit every 200 files)
    {
        let mut writer: IndexWriter = index.writer(50_000_000).unwrap();
        // Let natural merge policy run — that's what triggers the bug.

        for (i, (path, content)) in files.iter().enumerate() {
            writer.add_document(doc!(
                path_field => path.as_str(),
                content_field => content.as_str()
            )).unwrap();

            if (i + 1) % BATCH_SIZE == 0 {
                eprintln!("  commit at {}/{}", i + 1, files.len());
                writer.commit().unwrap();
            }
        }
        // Final commit
        writer.commit().unwrap();
        eprintln!("  final commit, total {} files", files.len());

        // Wait for merges to complete
        writer.wait_merging_threads().unwrap();
    }

    let reader = index.reader().unwrap();
    reader.reload().unwrap();
    let searcher = reader.searcher();

    let num_segments = searcher.segment_readers().len();
    let num_docs: u32 = searcher.segment_readers().iter().map(|r| r.num_docs()).sum();
    eprintln!("Index: {} docs in {} segments", num_docs, num_segments);

    // Test 1: startsWith prefix search with highlights on content field
    let term = Term::from_field_text(content_field, "fn");
    let query = FuzzyTermQuery::new_prefix(term, 0, true);
    let highlight_sink = Arc::new(HighlightSink::new());
    let query = query.with_highlight_sink(Arc::clone(&highlight_sink), "content".to_string());

    let top_docs = searcher.search(&query, &TopDocs::with_limit(20).order_by_score()).unwrap();
    eprintln!("startsWith 'fn': {} results", top_docs.len());
    assert!(!top_docs.is_empty(), "expected results for prefix 'fn'");

    // Verify highlights exist (this is what panicked before the fix)
    let mut highlighted = 0;
    for &(_score, doc_addr) in &top_docs {
        let seg_reader = searcher.segment_reader(doc_addr.segment_ord);
        let seg_id = seg_reader.segment_id();
        if highlight_sink.get(seg_id, doc_addr.doc_id).is_some() {
            highlighted += 1;
        }
    }
    eprintln!("  {} of {} results have highlights", highlighted, top_docs.len());
    assert!(highlighted > 0, "expected at least some highlights");

    // Test 2: another prefix
    let term2 = Term::from_field_text(content_field, "struct");
    let query2 = FuzzyTermQuery::new_prefix(term2, 0, true);
    let sink2 = Arc::new(HighlightSink::new());
    let query2 = query2.with_highlight_sink(Arc::clone(&sink2), "content".to_string());
    let top_docs2 = searcher.search(&query2, &TopDocs::with_limit(20).order_by_score()).unwrap();
    eprintln!("startsWith 'struct': {} results", top_docs2.len());
    assert!(!top_docs2.is_empty(), "expected results for prefix 'struct'");

    eprintln!("All startsWith + highlights tests passed!");
}
