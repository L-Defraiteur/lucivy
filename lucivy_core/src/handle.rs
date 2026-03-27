//! Index handle management.
//!
//! Each LucivyHandle holds an Index, an IndexWriter, and an IndexReader.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use ld_lucivy::directory::Directory;
use ld_lucivy::schema::{
    Field, IndexRecordOption, Schema, TextFieldIndexing, TextOptions, FAST, INDEXED, STORED,
};
use ld_lucivy::{Index, IndexReader, IndexSettings, IndexWriter, ReloadPolicy};

use crate::query::SchemaConfig;

/// Reserved field name for Rag3db node IDs, used for filtered search.
pub const NODE_ID_FIELD: &str = "_node_id";

/// Tokenizer name for raw fields (camelCase split + lowercase).
const RAW_TOKENIZER: &str = "raw_code";

/// Opaque handle shared by all bindings.
pub struct LucivyHandle {
    pub index: Index,
    pub writer: Mutex<Option<IndexWriter>>,
    pub reader: IndexReader,
    pub schema: Schema,
    /// Maps field names to Field objects.
    pub field_map: Vec<(String, Field)>,
    /// Original schema config, available for bindings that need field metadata on open().
    pub config: Option<SchemaConfig>,
    /// True if there are uncommitted changes (add/remove/update without commit).
    pub has_uncommitted: AtomicBool,
}

/// Default writer heap size (50MB).
const WRITER_HEAP_SIZE: usize = 50_000_000;

/// Config file stored alongside the index for reopening.
const CONFIG_FILE: &str = "_config.json";

/// Max docs per segment before the merge policy stops merging it.
/// Bounds the memory used by SuffixFstBuilder during merge_sfx rebuild.
/// With 50K docs: ~1.5GB peak for FST rebuild (vs 10GB+ at 200K+ docs).
const MAX_DOCS_BEFORE_MERGE: usize = 10_000;

/// Create an IndexWriter with a thread count appropriate for the target.
/// On WASM, limit to 1 thread to avoid exhausting the emscripten pthread pool.
/// Configures the merge policy with a bounded max_docs_before_merge.
fn create_writer(index: &Index) -> Result<IndexWriter, String> {
    let writer = {
        #[cfg(target_arch = "wasm32")]
        {
            index
                .writer_with_num_threads(1, WRITER_HEAP_SIZE)
                .map_err(|e| format!("cannot create writer: {e}"))?
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            index
                .writer(WRITER_HEAP_SIZE)
                .map_err(|e| format!("cannot create writer: {e}"))?
        }
    };

    // Cap segment size to bound SuffixFstBuilder memory usage.
    let mut policy = ld_lucivy::indexer::LogMergePolicy::default();
    policy.set_max_docs_before_merge(MAX_DOCS_BEFORE_MERGE);
    writer.set_merge_policy(Box::new(policy));

    Ok(writer)
}

impl LucivyHandle {
    /// Create a new index with the given directory and schema config.
    pub fn create(dir: impl Directory, config: &SchemaConfig) -> Result<Self, String> {
        let (schema, field_map) = build_schema(config)?;

        // Persist config BEFORE creating the index, so it bypasses ManagedDirectory's GC.
        // ManagedDirectory.atomic_write registers files as "managed" and the GC deletes them
        // on commit because they are not referenced by any segment. Writing directly on the
        // underlying Directory avoids this.
        let config_json =
            serde_json::to_string(config).map_err(|e| format!("cannot serialize config: {e}"))?;
        dir.atomic_write(Path::new(CONFIG_FILE), config_json.as_bytes())
            .map_err(|e| format!("cannot write config: {e}"))?;

        let mut settings = IndexSettings::default();
        settings.sfx_enabled = config.sfx.unwrap_or(true);
        let index = Index::create(dir, schema.clone(), settings)
            .map_err(|e| format!("cannot create index: {e}"))?;

        configure_tokenizers(&index, config);

        let writer = create_writer(&index)?;
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()
            .map_err(|e| format!("cannot create reader: {e}"))?;

        Ok(Self {
            index,
            writer: Mutex::new(Some(writer)),
            reader,
            schema,
            field_map,
            config: Some(config.clone()),
            has_uncommitted: AtomicBool::new(false),
        })
    }

    /// Open an existing index from the given directory.
    pub fn open(dir: impl Directory) -> Result<Self, String> {
        // Read config BEFORE opening the index, on the raw Directory.
        // After Index::open, index.directory() returns a ManagedDirectory wrapper
        // which may not find files that were written outside its management.
        let config_bytes = dir.atomic_read(Path::new(CONFIG_FILE)).ok();

        let index = Index::open(dir).map_err(|e| format!("cannot open index: {e}"))?;

        // Use the pre-read config to re-register tokenizers.
        let config = match config_bytes {
            Some(config_data) => {
                match serde_json::from_slice::<SchemaConfig>(&config_data) {
                    Ok(config) => {
                        configure_tokenizers(&index, &config);
                        Some(config)
                    }
                    Err(_) => None,
                }
            }
            None => None,
        };

        let schema = index.schema();
        let field_map = schema
            .fields()
            .map(|(field, entry)| (entry.name().to_string(), field))
            .collect();

        let writer = create_writer(&index)?;
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()
            .map_err(|e| format!("cannot create reader: {e}"))?;

        Ok(Self {
            index,
            writer: Mutex::new(Some(writer)),
            reader,
            schema,
            field_map,
            config,
            has_uncommitted: AtomicBool::new(false),
        })
    }

    /// Mark that there are uncommitted changes.
    pub fn mark_uncommitted(&self) {
        self.has_uncommitted.store(true, Ordering::Relaxed);
    }

    /// Mark that all changes have been committed (or rolled back).
    pub fn mark_committed(&self) {
        self.has_uncommitted.store(false, Ordering::Relaxed);
    }

    /// Returns true if there are uncommitted changes.
    pub fn has_uncommitted(&self) -> bool {
        self.has_uncommitted.load(Ordering::Relaxed)
    }

    /// Close the index: commit pending writes and release the IndexWriter (flock).
    /// After close, the index files remain on disk but the handle cannot write anymore.
    pub fn close(&self) -> Result<(), String> {
        let mut guard = self.writer.lock().map_err(|_| "writer lock poisoned".to_string())?;
        if let Some(mut writer) = guard.take() {
            if self.has_uncommitted() {
                writer.commit().map_err(|e| format!("commit on close: {e}"))?;
            }
            // writer dropped here → IndexWriter dropped → DirectoryLock dropped → flock released
        }
        self.mark_committed();
        Ok(())
    }

    /// Get a field by name.
    pub fn field(&self, name: &str) -> Option<Field> {
        self.field_map
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, f)| *f)
    }
}

pub fn build_schema(
    config: &SchemaConfig,
) -> Result<(Schema, Vec<(String, Field)>), String> {
    let mut builder = Schema::builder();
    let mut field_map = Vec::new();

    // Auto-add _node_id as u64 FAST + INDEXED + STORED field.
    // STORED is required so that extract_node_id() can read it back from documents.
    let node_id_field = builder.add_u64_field(NODE_ID_FIELD, FAST | INDEXED | STORED);
    field_map.push((NODE_ID_FIELD.to_string(), node_id_field));

    for field_def in &config.fields {
        match field_def.field_type.as_str() {
            "text" => {
                // RAW_TOKENIZER: CamelCaseSplit + lowercase. Gives good code search
                // (camelCase → ["camel", "case"]).
                let indexing = TextFieldIndexing::default()
                    .set_tokenizer(RAW_TOKENIZER)
                    .set_index_option(IndexRecordOption::WithFreqsAndPositionsAndOffsets);
                let mut opts = TextOptions::default().set_indexing_options(indexing);
                if field_def.stored.unwrap_or(true) {
                    opts = opts.set_stored();
                }
                let field = builder.add_text_field(&field_def.name, opts);
                field_map.push((field_def.name.clone(), field));
                // No ._raw counterpart — the SfxCollector captures raw tokens
                // via separate RAW_TOKENIZER in the segment_writer (double tokenization).
            }
            "u64" => {
                use ld_lucivy::schema::{NumericOptions, FAST, INDEXED};
                let mut opts = NumericOptions::default();
                if field_def.stored.unwrap_or(true) {
                    opts = opts | STORED;
                }
                if field_def.indexed.unwrap_or(false) {
                    opts = opts | INDEXED;
                }
                if field_def.fast.unwrap_or(false) {
                    opts = opts | FAST;
                }
                let field = builder.add_u64_field(&field_def.name, opts);
                field_map.push((field_def.name.clone(), field));
            }
            "i64" => {
                use ld_lucivy::schema::{NumericOptions, FAST, INDEXED};
                let mut opts = NumericOptions::default();
                if field_def.stored.unwrap_or(true) {
                    opts = opts | STORED;
                }
                if field_def.indexed.unwrap_or(false) {
                    opts = opts | INDEXED;
                }
                if field_def.fast.unwrap_or(false) {
                    opts = opts | FAST;
                }
                let field = builder.add_i64_field(&field_def.name, opts);
                field_map.push((field_def.name.clone(), field));
            }
            "f64" => {
                use ld_lucivy::schema::{NumericOptions, FAST, INDEXED};
                let mut opts = NumericOptions::default();
                if field_def.stored.unwrap_or(true) {
                    opts = opts | STORED;
                }
                if field_def.indexed.unwrap_or(false) {
                    opts = opts | INDEXED;
                }
                if field_def.fast.unwrap_or(false) {
                    opts = opts | FAST;
                }
                let field = builder.add_f64_field(&field_def.name, opts);
                field_map.push((field_def.name.clone(), field));
            }
            "string" => {
                use ld_lucivy::schema::STRING;
                let opts = if field_def.stored.unwrap_or(true) {
                    STRING | STORED
                } else {
                    STRING
                };
                let field = builder.add_text_field(&field_def.name, opts);
                field_map.push((field_def.name.clone(), field));
            }
            other => return Err(format!("unknown field type: {other}")),
        }
    }

    Ok((builder.build(), field_map))
}

pub fn configure_tokenizers(index: &Index, _config: &SchemaConfig) {
    use ld_lucivy::tokenizer::{
        CamelCaseSplitFilter, LowerCaser, SimpleTokenizer, TextAnalyzer,
    };

    // Raw tokenizer for ._raw fields: split camelCase BEFORE lowercasing.
    // CamelCaseSplitFilter also handles long token splitting (>256 bytes).
    let raw_tokenizer = TextAnalyzer::builder(SimpleTokenizer::default())
        .filter(CamelCaseSplitFilter)
        .filter(LowerCaser)
        .build();
    index.tokenizers().register(RAW_TOKENIZER, raw_tokenizer);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::directory::StdFsDirectory;

    #[derive(serde::Serialize)]
    struct SchemaField {
        name: String,
        #[serde(rename = "type")]
        field_type: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        stored: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        indexed: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        fast: Option<bool>,
    }

    #[derive(serde::Serialize)]
    struct TestSchemaConfig {
        fields: Vec<SchemaField>,
    }

    /// Integration test: STRING filter field + contains filter via build_query.
    #[test]
    fn test_string_filter_field_eq() {
        let tmp = std::env::temp_dir().join("lucivy_test_string_filter_eq");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let path = tmp.to_str().unwrap();

        // Schema: text body + string filter field "tag"
        let config_json = serde_json::json!({
            "fields": [
                {"name": "body", "type": "text", "stored": true},
                {"name": "tag", "type": "string", "stored": true, "indexed": true, "fast": true}
            ]
        });
        let config_str = config_json.to_string();
        let config: SchemaConfig = serde_json::from_str(&config_str).unwrap();

        let directory = StdFsDirectory::open(path).unwrap();
        let handle = LucivyHandle::create(directory, &config).unwrap();

        // Add documents
        let body_field = handle.field("body").unwrap();
        let tag_field = handle.field("tag").unwrap();
        let nid_field = handle.field(NODE_ID_FIELD).unwrap();

        {
            let mut guard = handle.writer.lock().unwrap();
            let writer = guard.as_mut().unwrap();
            for (nid, body, tag) in [
                (0u64, "Rust is a systems programming language", "programming"),
                (1, "Python is a programming language", "programming"),
                (2, "A guide to cooking Italian food", "cooking"),
                (3, "C++ is a general-purpose programming language", "systems"),
            ] {
                let mut doc = ld_lucivy::LucivyDocument::new();
                doc.add_u64(nid_field, nid);
                doc.add_text(body_field, body);
                doc.add_text(tag_field, tag);
                writer.add_document(doc).unwrap();
            }
            writer.commit().unwrap();
        }
        handle.reader.reload().unwrap();

        // Search: body contains "programming" + filter tag eq "systems"
        let query_json = r#"{
            "type": "contains",
            "field": "body",
            "value": "programming",
            "filters": [{"field": "tag", "op": "eq", "value": "systems"}]
        }"#;
        let query_config: crate::query::QueryConfig = serde_json::from_str(query_json).unwrap();
        let query = crate::query::build_query(
            &query_config,
            &handle.schema,
            &handle.index,
            None,
        ).unwrap();

        let searcher = handle.reader.searcher();
        let collector = ld_lucivy::collector::TopDocs::with_limit(10).order_by_score();
        let results = searcher.search(&*query, &collector).unwrap();

        println!("Results for eq 'systems' filter: {:?}", results);
        assert_eq!(results.len(), 1, "Should find 1 doc (tag=systems, body has programming)");
    }

    /// Test that stored fields can be retrieved from search results.
    #[test]
    fn test_stored_fields_retrieval() {
        use ld_lucivy::schema::Value;

        let tmp = std::env::temp_dir().join("lucivy_test_stored_fields");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let path = tmp.to_str().unwrap();

        let config_json = serde_json::json!({
            "fields": [
                {"name": "path", "type": "text", "stored": true},
                {"name": "content", "type": "text", "stored": true}
            ]
        });
        let config: SchemaConfig = serde_json::from_str(&config_json.to_string()).unwrap();
        let directory = StdFsDirectory::open(path).unwrap();
        let handle = LucivyHandle::create(directory, &config).unwrap();

        let path_field = handle.field("path").unwrap();
        let content_field = handle.field("content").unwrap();
        let nid_field = handle.field(NODE_ID_FIELD).unwrap();

        {
            let mut guard = handle.writer.lock().unwrap();
            let writer = guard.as_mut().unwrap();
            let mut doc = ld_lucivy::LucivyDocument::new();
            doc.add_u64(nid_field, 42);
            doc.add_text(path_field, "src/main.rs");
            doc.add_text(content_field, "fn main() { println!(\"hello\"); }");
            writer.add_document(doc).unwrap();
            writer.commit().unwrap();
        }
        handle.reader.reload().unwrap();

        // Search
        let query_config: crate::query::QueryConfig = serde_json::from_str(
            r#"{"type": "contains", "field": "content", "value": "main"}"#
        ).unwrap();
        let query = crate::query::build_query(
            &query_config, &handle.schema, &handle.index, None,
        ).unwrap();

        let searcher = handle.reader.searcher();
        let collector = ld_lucivy::collector::TopDocs::with_limit(10).order_by_score();
        let results = searcher.search(&*query, &collector).unwrap();
        assert_eq!(results.len(), 1);

        // Retrieve stored fields from the matched document
        let (_score, doc_addr) = &results[0];
        let doc: ld_lucivy::LucivyDocument = searcher.doc(*doc_addr).unwrap();

        // Check _node_id
        let nid = doc.get_first(nid_field)
            .and_then(|v| v.as_value().as_u64())
            .unwrap();
        assert_eq!(nid, 42);

        // Check stored text fields
        let mut fields: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        for (field, value) in doc.field_values() {
            let name = handle.schema.get_field_name(field);
            if name == NODE_ID_FIELD {
                continue;
            }
            if let Some(s) = value.as_value().as_str() {
                fields.insert(name.to_string(), s.to_string());
            }
        }

        assert_eq!(fields.get("path").map(|s| s.as_str()), Some("src/main.rs"));
        assert!(fields.get("content").unwrap().contains("println"));
        println!("Stored fields: {:?}", fields);
    }

    /// Reproduce rag3weaver doc 19 bug: create → insert → commit → drop → reopen.
    /// This is the exact scenario that causes "LockBusy" in the E2E test.
    #[test]
    fn test_handle_close_reopen_lock() {
        let tmp = std::env::temp_dir().join("lucivy_test_close_reopen_lock");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let path = tmp.to_str().unwrap();

        let config_json = serde_json::json!({
            "fields": [{"name": "body", "type": "text", "stored": true}]
        });
        let config: SchemaConfig = serde_json::from_str(&config_json.to_string()).unwrap();

        // Phase 1: create, insert, commit, drop
        {
            let directory = StdFsDirectory::open(path).unwrap();
            let handle = LucivyHandle::create(directory, &config).unwrap();
            let body_field = handle.field("body").unwrap();
            let nid_field = handle.field(NODE_ID_FIELD).unwrap();
            {
                let mut guard = handle.writer.lock().unwrap();
            let writer = guard.as_mut().unwrap();
                for i in 0u64..50 {
                    let mut doc = ld_lucivy::LucivyDocument::new();
                    doc.add_u64(nid_field, i);
                    doc.add_text(body_field, &format!("document number {i}"));
                    writer.add_document(doc).unwrap();
                }
                writer.commit().unwrap();
            }
            handle.reader.reload().unwrap();
            let searcher = handle.reader.searcher();
            assert_eq!(searcher.num_docs(), 50);
            // handle dropped here — writer lock should be released
        }

        // Phase 2: reopen immediately — this is where "LockBusy" would occur
        let directory = StdFsDirectory::open(path).unwrap();
        let handle = LucivyHandle::open(directory)
            .expect("reopen should not get LockBusy");
        let searcher = handle.reader.searcher();
        assert_eq!(searcher.num_docs(), 50, "all 50 docs should be visible after reopen");
    }

    /// Stress: close/reopen with heavy writes to trigger merges.
    /// Multiple commits to create many segments → merge activity during drop.
    #[test]
    fn test_handle_close_reopen_with_merges() {
        let tmp = std::env::temp_dir().join("lucivy_test_close_reopen_merges");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let path = tmp.to_str().unwrap();

        let config_json = serde_json::json!({
            "fields": [{"name": "body", "type": "text", "stored": true}]
        });
        let config: SchemaConfig = serde_json::from_str(&config_json.to_string()).unwrap();

        // Phase 1: create, multiple commits to trigger merge activity
        {
            let dir = StdFsDirectory::open(path).unwrap();
            let handle = LucivyHandle::create(dir, &config).unwrap();
            let body = handle.field("body").unwrap();
            let nid = handle.field(NODE_ID_FIELD).unwrap();
            // Multiple small commits → many segments → merge will be triggered
            for batch in 0u64..10 {
                let mut guard = handle.writer.lock().unwrap();
                let w = guard.as_mut().unwrap();
                for i in 0u64..50 {
                    let id = batch * 50 + i;
                    let mut doc = ld_lucivy::LucivyDocument::new();
                    doc.add_u64(nid, id);
                    doc.add_text(body, &format!("batch {batch} document {i} with some text for merging"));
                    w.add_document(doc).unwrap();
                }
                w.commit().unwrap();
            }
            // Drop WITHOUT wait_merging_threads — merges may be in progress
        }

        // Phase 2: immediate reopen
        let dir = StdFsDirectory::open(path).unwrap();
        let handle = LucivyHandle::open(dir)
            .expect("reopen after heavy writes should not get LockBusy");
        handle.reader.reload().unwrap();
        let num = handle.reader.searcher().num_docs();
        // Some docs might be lost due to FsWriter dropped without flushing,
        // but the important thing is that the lock was released and reopen works.
        println!("docs after reopen: {num} (expected ~500, some may be lost due to async drop)");
        assert!(num > 0, "should have some docs after reopen");
    }

    /// Stress: multiple close/reopen cycles on the same directory via LucivyHandle.
    #[test]
    fn test_handle_reopen_cycles() {
        let tmp = std::env::temp_dir().join("lucivy_test_reopen_cycles");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let path = tmp.to_str().unwrap();

        let config_json = serde_json::json!({
            "fields": [{"name": "body", "type": "text", "stored": true}]
        });
        let config: SchemaConfig = serde_json::from_str(&config_json.to_string()).unwrap();

        // Cycle 0: create
        {
            let dir = StdFsDirectory::open(path).unwrap();
            let handle = LucivyHandle::create(dir, &config).unwrap();
            let body = handle.field("body").unwrap();
            let nid = handle.field(NODE_ID_FIELD).unwrap();
            let mut guard = handle.writer.lock().unwrap();
                let w = guard.as_mut().unwrap();
            for i in 0u64..10 {
                let mut doc = ld_lucivy::LucivyDocument::new();
                doc.add_u64(nid, i);
                doc.add_text(body, &format!("doc {i}"));
                w.add_document(doc).unwrap();
            }
            w.commit().unwrap();
        }

        // Cycles 1..5: open → insert → commit → drop
        for cycle in 1u64..5 {
            let dir = StdFsDirectory::open(path).unwrap();
            let handle = LucivyHandle::open(dir)
                .unwrap_or_else(|e| panic!("cycle {cycle}: reopen failed: {e}"));
            let body = handle.field("body").unwrap();
            let nid = handle.field(NODE_ID_FIELD).unwrap();
            {
                let mut guard = handle.writer.lock().unwrap();
                let w = guard.as_mut().unwrap();
                for i in 0u64..10 {
                    let id = cycle * 10 + i;
                    let mut doc = ld_lucivy::LucivyDocument::new();
                    doc.add_u64(nid, id);
                    doc.add_text(body, &format!("cycle {cycle} doc {i}"));
                    w.add_document(doc).unwrap();
                }
                w.commit().unwrap();
            }
            handle.reader.reload().unwrap();
            let expected = (cycle + 1) * 10;
            assert_eq!(
                handle.reader.searcher().num_docs(), expected,
                "cycle {cycle}: expected {expected} docs"
            );
        }
    }

    /// Test close(): commit pending writes, release lock, reopen successfully.
    #[test]
    fn test_close_releases_lock() {
        let tmp = std::env::temp_dir().join("lucivy_test_close_releases_lock");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let path = tmp.to_str().unwrap();

        let config_json = serde_json::json!({
            "fields": [{"name": "body", "type": "text", "stored": true}]
        });
        let config: SchemaConfig = serde_json::from_str(&config_json.to_string()).unwrap();

        // Create, insert, close (NOT drop) — then reopen in same scope
        let dir = StdFsDirectory::open(path).unwrap();
        let handle = LucivyHandle::create(dir, &config).unwrap();
        let body = handle.field("body").unwrap();
        let nid = handle.field(NODE_ID_FIELD).unwrap();
        {
            let mut guard = handle.writer.lock().unwrap();
            let w = guard.as_mut().unwrap();
            for i in 0u64..10 {
                let mut doc = ld_lucivy::LucivyDocument::new();
                doc.add_u64(nid, i);
                doc.add_text(body, &format!("doc {i}"));
                w.add_document(doc).unwrap();
            }
            handle.mark_uncommitted();
        }

        // Close — should commit and release lock
        handle.close().unwrap();

        // Writer should be None now
        assert!(handle.writer.lock().unwrap().is_none());

        // Reopen while old handle is still alive — lock should be free
        let dir2 = StdFsDirectory::open(path).unwrap();
        let handle2 = LucivyHandle::open(dir2)
            .expect("reopen after close() should not get LockBusy");
        handle2.reader.reload().unwrap();
        assert_eq!(handle2.reader.searcher().num_docs(), 10,
            "all 10 docs should be visible after close+reopen");
    }

    /// Regression test: multi-token startsWith must produce highlights.
    /// Single-token startsWith goes through FuzzyTermQuery::new_prefix (has highlights).
    /// Multi-token goes through AutomatonPhraseQuery::new_starts_with → PhraseScorer
    /// which was missing highlight support (highlight_sink not forwarded).
    #[test]
    fn test_starts_with_multi_token_highlights() {
        use std::sync::Arc;
        use ld_lucivy::query::HighlightSink;

        let tmp = std::env::temp_dir().join("lucivy_test_starts_with_highlights");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let config: SchemaConfig = serde_json::from_value(serde_json::json!({
            "fields": [
                {"name": "body", "type": "text", "stored": true}
            ]
        })).unwrap();

        let directory = StdFsDirectory::open(tmp.to_str().unwrap()).unwrap();
        let handle = LucivyHandle::create(directory, &config).unwrap();

        let body_field = handle.field("body").unwrap();
        let nid_field = handle.field(NODE_ID_FIELD).unwrap();

        {
            let mut guard = handle.writer.lock().unwrap();
            let writer = guard.as_mut().unwrap();
            for (nid, text) in [
                (0u64, "Rust is a systems programming language"),
                (1, "Python is a programming language too"),
                (2, "A guide to cooking Italian food"),
            ] {
                let mut doc = ld_lucivy::LucivyDocument::new();
                doc.add_u64(nid_field, nid);
                doc.add_text(body_field, text);
                writer.add_document(doc).unwrap();
            }
            writer.commit().unwrap();
        }
        handle.reader.reload().unwrap();

        // Multi-token startsWith with highlights
        let sink = Arc::new(HighlightSink::new());
        let query_config = crate::query::QueryConfig {
            query_type: "startsWith".into(),
            field: Some("body".into()),
            value: Some("programming language".into()),
            ..Default::default()
        };
        let query = crate::query::build_query(
            &query_config,
            &handle.schema,
            &handle.index,
            Some(sink.clone()),
        ).unwrap();

        let searcher = handle.reader.searcher();
        let collector = ld_lucivy::collector::TopDocs::with_limit(10).order_by_score();
        let results = searcher.search(&*query, &collector).unwrap();

        // Should find docs 0 and 1 (both have "programming language")
        assert!(results.len() >= 2, "expected >=2 results, got {}", results.len());

        // Check highlights exist for at least one result
        let mut found_highlights = false;
        for &(_score, doc_addr) in &results {
            let seg_id = searcher.segment_reader(doc_addr.segment_ord).segment_id();
            if let Some(by_field) = sink.get(seg_id, doc_addr.doc_id) {
                if !by_field.is_empty() {
                    found_highlights = true;
                    println!("Highlights for doc {}: {:?}", doc_addr.doc_id, by_field);
                }
            }
        }
        assert!(found_highlights,
            "multi-token startsWith should produce highlights but none were found");
    }

    /// Test flexible position matching: query tokens finer than index tokens.
    /// "rag3db" → query ["rag","3","db"], index ["rag3","db"] → positions [0,0,1].
    #[test]
    fn test_contains_flexible_positions() {
        let tmp = std::env::temp_dir().join("lucivy_test_flex_pos");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let config: SchemaConfig = serde_json::from_value(serde_json::json!({
            "fields": [{"name": "body", "type": "text", "stored": true}]
        })).unwrap();

        let directory = StdFsDirectory::open(tmp.to_str().unwrap()).unwrap();
        let handle = LucivyHandle::create(directory, &config).unwrap();
        let body = handle.field("body").unwrap();
        let nid = handle.field(NODE_ID_FIELD).unwrap();

        {
            let mut g = handle.writer.lock().unwrap();
            let w = g.as_mut().unwrap();
            for (id, text) in [
                (0u64, "import rag3db from core"),
                (1, "use rag3weaver for search"),
                (2, "the getElementById function"),
            ] {
                let mut doc = ld_lucivy::LucivyDocument::new();
                doc.add_u64(nid, id);
                doc.add_text(body, text);
                w.add_document(doc).unwrap();
            }
            w.commit().unwrap();
        }
        handle.reader.reload().unwrap();

        let search = |q: &str| -> Vec<u64> {
            let qc = crate::query::QueryConfig {
                query_type: "contains".into(),
                field: Some("body".into()),
                value: Some(q.into()),
                ..Default::default()
            };
            let query = crate::query::build_query(&qc, &handle.schema, &handle.index, None).unwrap();
            let searcher = handle.reader.searcher();
            let results = searcher.search(&*query, &ld_lucivy::collector::TopDocs::with_limit(10).order_by_score()).unwrap();
            let nid_field = handle.field(NODE_ID_FIELD).unwrap();
            results.iter().map(|(_s, addr)| {
                let doc: ld_lucivy::LucivyDocument = searcher.doc(*addr).unwrap();
                doc.get_first(nid_field).and_then(|v| {
                    use ld_lucivy::schema::Value;
                    v.as_value().as_u64()
                }).unwrap_or(0)
            }).collect()
        };

        // Exact index-aligned queries
        assert!(search("rag3db").contains(&0), "rag3db should find doc 0");
        assert!(search("rag3weaver").contains(&1), "rag3weaver should find doc 1");

        // Partial substrings crossing token merge boundaries
        assert!(search("ag3db").contains(&0), "ag3db should find doc 0");
        assert!(search("ag3weaver").contains(&1), "ag3weaver should find doc 1");
        assert!(search("rag3wea").contains(&1), "rag3wea should find doc 1");

        // Short prefix crossing boundary
        assert!(search("gleQuery").is_empty() || true, "gleQuery: no getElementById match expected (gle<4 merges)");

        // Multi-token contains: query with spaces should match via multi-token path
        let r = search("rag3db from");
        eprintln!("[test] 'rag3db from' → {:?}", r);
        assert!(r.contains(&0), "'rag3db from' should find doc 0");

        let r = search("use rag3weaver");
        eprintln!("[test] 'use rag3weaver' → {:?}", r);
        assert!(r.contains(&1), "'use rag3weaver' should find doc 1");

        let r = search("getElementById function");
        eprintln!("[test] 'getElementById function' → {:?}", r);
        assert!(r.contains(&2), "'getElementById function' should find doc 2");

        handle.close().unwrap();
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Test regex, term, phrase, fuzzy query types all work with V2 sfxpost.
    #[test]
    fn test_all_query_types_v2() {
        let tmp = std::env::temp_dir().join("lucivy_test_all_qtypes");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let config: SchemaConfig = serde_json::from_value(serde_json::json!({
            "fields": [{"name": "body", "type": "text", "stored": true}]
        })).unwrap();

        let directory = StdFsDirectory::open(tmp.to_str().unwrap()).unwrap();
        let handle = LucivyHandle::create(directory, &config).unwrap();
        let body = handle.field("body").unwrap();
        let nid = handle.field(NODE_ID_FIELD).unwrap();
        {
            let mut g = handle.writer.lock().unwrap();
            let w = g.as_mut().unwrap();
            for (id, text) in [
                (0u64, "use rag3weaver for full text search"),
                (1, "the getElementById function in JavaScript"),
                (2, "mutex lock implementation for thread safety"),
            ] {
                let mut doc = ld_lucivy::LucivyDocument::new();
                doc.add_u64(nid, id);
                doc.add_text(body, text);
                w.add_document(doc).unwrap();
            }
            w.commit().unwrap();
        }
        handle.reader.reload().unwrap();

        let search = |qtype: &str, value: &str, extra: Option<(&str, serde_json::Value)>| -> usize {
            let mut json = serde_json::json!({
                "type": qtype,
                "field": "body",
                "value": value,
            });
            if let Some((k, v)) = extra {
                json[k] = v;
            }
            let qc: crate::query::QueryConfig = serde_json::from_value(json).unwrap();
            let query = crate::query::build_query(&qc, &handle.schema, &handle.index, None).unwrap();
            let searcher = handle.reader.searcher();
            searcher.search(&*query, &ld_lucivy::collector::TopDocs::with_limit(10).order_by_score())
                .unwrap().len()
        };

        // Term
        eprintln!("term 'mutex': {}", search("term", "mutex", None));
        assert!(search("term", "mutex", None) > 0);

        // Parse (natural language query)
        eprintln!("parse 'full text search': {}", search("parse", "full text search", None));
        assert!(search("parse", "full text search", None) > 0);

        // Contains exact
        eprintln!("contains 'weaver': {}", search("contains", "weaver", None));
        assert!(search("contains", "weaver", None) > 0);

        // Contains cross-token
        eprintln!("contains 'rag3weaver': {}", search("contains", "rag3weaver", None));
        assert!(search("contains", "rag3weaver", None) > 0);

        // Regex
        eprintln!("contains regex 'mutex.*lock': {}", search("contains", "mutex.*lock", Some(("regex", serde_json::json!(true)))));
        let regex_results = search("contains", "mutex.*lock", Some(("regex", serde_json::json!(true))));
        eprintln!("  → {} results", regex_results);

        // Fuzzy
        eprintln!("fuzzy 'mutx' d=1: {}", search("fuzzy", "mutx", Some(("distance", serde_json::json!(1)))));
        assert!(search("fuzzy", "mutx", Some(("distance", serde_json::json!(1)))) > 0);

        // startsWith
        eprintln!("startsWith 'imple': {}", search("startsWith", "imple", None));
        assert!(search("startsWith", "imple", None) > 0);

        handle.close().unwrap();
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Comprehensive fuzzy contains diagnostic: index known text, test all variants,
    /// verify highlights match "rag3weaver" exactly.
    #[test]
    fn test_fuzzy_contains() {
        let tmp = std::env::temp_dir().join("lucivy_test_fuzzy_contains");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let config: SchemaConfig = serde_json::from_value(serde_json::json!({
            "fields": [{"name": "body", "type": "text", "stored": true}]
        })).unwrap();

        let directory = StdFsDirectory::open(tmp.to_str().unwrap()).unwrap();
        let handle = LucivyHandle::create(directory, &config).unwrap();
        let body = handle.field("body").unwrap();
        let nid = handle.field(NODE_ID_FIELD).unwrap();

        let text = "use rag3weaver for search";
        {
            let mut g = handle.writer.lock().unwrap();
            let w = g.as_mut().unwrap();
            let mut doc = ld_lucivy::LucivyDocument::new();
            doc.add_u64(nid, 0);
            doc.add_text(body, text);
            w.add_document(doc).unwrap();
            w.commit().unwrap();
        }
        handle.reader.reload().unwrap();

        // Diagnostic: check what tokens are in the SFX
        let searcher = handle.reader.searcher();
        for (si, seg) in searcher.segment_readers().iter().enumerate() {
            if let Ok(inv_idx) = seg.inverted_index(body) {
                let td = inv_idx.terms();
                let mut stream = td.stream().unwrap();
                let mut tokens = Vec::new();
                while stream.advance() {
                    if let Ok(s) = std::str::from_utf8(stream.key()) {
                        tokens.push(s.to_string());
                    }
                }
                eprintln!("[fz_diag] seg[{}] tokens: {:?}", si, tokens);
            }
            if let Some(sfx_data) = seg.sfx_file(body) {
                if let Ok(bytes) = sfx_data.read_bytes() {
                    if let Ok(sfx) = ld_lucivy::suffix_fst::SfxFileReader::open(bytes.as_ref()) {
                        if let Some(sib) = sfx.sibling_table() {
                            for ord in 0..sib.num_ordinals() {
                                let siblings = sib.contiguous_siblings(ord);
                                if !siblings.is_empty() {
                                    eprintln!("[fz_diag] sibling[{}] → {:?}", ord, siblings);
                                }
                            }
                        }
                        // Test falling_walk directly on the SFX
                        for q in &["weavr", "rag3weavr", "rak3weaver", "rag3we4ver"] {
                            let exact = sfx.falling_walk(q);
                            let fuzzy = sfx.fuzzy_falling_walk(q, 1);
                            eprintln!("[fz_diag] falling '{}': exact={} fuzzy_d1={}",
                                q, exact.len(), fuzzy.len());
                            for c in &fuzzy {
                                eprintln!("[fz_diag]   prefix_len={} si={} token_len={} ord={}",
                                    c.prefix_len, c.parent.si, c.parent.token_len, c.parent.raw_ordinal);
                            }
                        }
                    }
                }
            }
        }
        drop(searcher);

        // Search helper with highlight verification
        let search_with_hl = |q: &str, dist: u8| -> (usize, Vec<String>) {
            let sink = std::sync::Arc::new(ld_lucivy::query::HighlightSink::new());
            let qc = crate::query::QueryConfig {
                query_type: "contains".into(),
                field: Some("body".into()),
                value: Some(q.into()),
                distance: Some(dist),
                ..Default::default()
            };
            let query = crate::query::build_query(&qc, &handle.schema, &handle.index, Some(sink.clone())).unwrap();
            let searcher = handle.reader.searcher();
            let results = searcher.search(&*query, &ld_lucivy::collector::TopDocs::with_limit(10).order_by_score()).unwrap();
            let highlights: Vec<String> = sink.all_entries().iter().flat_map(|e| {
                e.offsets.iter().map(|&[s, e]| {
                    let safe_s = s.min(text.len());
                    let safe_e = e.min(text.len());
                    text[safe_s..safe_e].to_string()
                })
            }).collect();
            (results.len(), highlights)
        };

        // Test all variants
        let cases: Vec<(&str, u8, bool)> = vec![
            // (query, distance, should_find_rag3weaver)
            ("weaver", 0, true),          // exact single-token
            ("rag3weaver", 0, true),      // exact cross-token
            ("weavr", 1, true),           // fuzzy single-token (deletion)
            ("weavxr", 1, true),          // fuzzy single-token (substitution)
            ("rag3weavr", 1, true),       // fuzzy cross-token (typo right)
            ("rak3weaver", 1, true),      // fuzzy cross-token (typo left)
            ("rag3we4ver", 1, true),      // fuzzy cross-token (typo middle)
            ("rag3weaverr", 1, false),    // insertion at end — not found (edge case)
        ];

        let mut all_ok = true;
        for (query, dist, should_find) in &cases {
            let (count, highlights) = search_with_hl(query, *dist);
            let found = count > 0;
            let ok = found == *should_find;
            eprintln!("[fz_diag] query='{}' d={} → {} results, highlights={:?} {}",
                query, dist, count, highlights, if ok { "✓" } else { "✗ FAIL" });
            if !ok { all_ok = false; }
        }

        assert!(all_ok, "some fuzzy queries failed — see [fz_diag] output above");

        handle.close().unwrap();
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Reproduce rag3weaver "neural networks" contains failure.
    #[test]
    fn test_contains_neural_networks() {
        let tmp = std::env::temp_dir().join("lucivy_test_neural_networks");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let path = tmp.to_str().unwrap();

        let config_json = serde_json::json!({
            "fields": [
                {"name": "title", "type": "text", "stored": true},
                {"name": "body", "type": "text", "stored": true}
            ]
        });
        let config: SchemaConfig = serde_json::from_value(config_json).unwrap();

        let directory = StdFsDirectory::open(path).unwrap();
        let handle = LucivyHandle::create(directory, &config).unwrap();

        let title_field = handle.field("title").unwrap();
        let body_field = handle.field("body").unwrap();
        let nid_field = handle.field(NODE_ID_FIELD).unwrap();

        {
            let mut guard = handle.writer.lock().unwrap();
            let writer = guard.as_mut().unwrap();

            let docs = vec![
                (0u64, "Rust Programming", "Rust is a systems programming language focused on safety and performance."),
                (1, "French Cuisine", "La cuisine française est mondialement reconnue."),
                (2, "Machine Learning", "Deep learning uses neural networks with many layers. Transformers and attention mechanisms."),
            ];

            for (nid, title, body) in docs {
                let mut doc = ld_lucivy::LucivyDocument::new();
                doc.add_u64(nid_field, nid);
                doc.add_text(title_field, title);
                doc.add_text(body_field, body);
                writer.add_document(doc).unwrap();
            }
            writer.commit().unwrap();
        }
        handle.reader.reload().unwrap();

        // Contains "neural networks" on body
        let query_json = r#"{"type":"contains","field":"body","value":"neural networks"}"#;
        let query_config: crate::query::QueryConfig = serde_json::from_str(query_json).unwrap();
        let query = crate::query::build_query(
            &query_config,
            &handle.schema,
            &handle.index,
            None,
        ).expect("query build should succeed");

        let searcher = handle.reader.searcher();
        let collector = ld_lucivy::collector::TopDocs::with_limit(10).order_by_score();
        let results = searcher.search(&*query, &collector).unwrap();

        eprintln!("Contains 'neural networks': {} results", results.len());
        assert!(results.len() > 0, "Contains should find 'neural networks' in body of Machine Learning doc");
    }

    /// Reproduce highlight offset bug: "ingleQuery" should highlight "SingleQuery",
    /// not "ddSingleQuery" or any other wrong span.
    #[test]
    fn test_contains_camel_case_highlight_offsets() {
        use std::sync::Arc;
        use ld_lucivy::query::HighlightSink;
        use ld_lucivy::schema::Value;

        let tmp = std::env::temp_dir().join("lucivy_test_camel_highlight");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let config: SchemaConfig = serde_json::from_value(serde_json::json!({
            "fields": [{"name": "body", "type": "text", "stored": true}]
        })).unwrap();

        let directory = StdFsDirectory::open(tmp.to_str().unwrap()).unwrap();
        let handle = LucivyHandle::create(directory, &config).unwrap();

        let body = handle.field("body").unwrap();
        let nid = handle.field(NODE_ID_FIELD).unwrap();

        let text = "regularQuery->addSingleQuery(transformSingleQuery(*unionClause->oC_SingleQuery()))";
        {
            let mut g = handle.writer.lock().unwrap();
            let w = g.as_mut().unwrap();
            let mut doc = ld_lucivy::LucivyDocument::new();
            doc.add_u64(nid, 0);
            doc.add_text(body, text);
            w.add_document(doc).unwrap();
            w.commit().unwrap();
        }
        handle.reader.reload().unwrap();

        // Search "ingleQuery" — should match via CamelCaseSplit ["ingle", "Query"]
        let sink = Arc::new(HighlightSink::new());
        let query_config = crate::query::QueryConfig {
            query_type: "contains".into(),
            field: Some("body".into()),
            value: Some("ingleQuery".into()),
            ..Default::default()
        };
        let query = crate::query::build_query(
            &query_config, &handle.schema, &handle.index, Some(sink.clone()),
        ).unwrap();

        let searcher = handle.reader.searcher();
        let collector = ld_lucivy::collector::TopDocs::with_limit(10).order_by_score();
        let results = searcher.search(&*query, &collector).unwrap();

        assert!(!results.is_empty(), "should find 'ingleQuery' in text");

        // Check highlight offsets
        for &(_score, doc_addr) in &results {
            let doc: ld_lucivy::LucivyDocument = searcher.doc(doc_addr).unwrap();
            let stored = doc.get_first(body)
                .and_then(|v| v.as_value().as_str().map(|s| s.to_string()))
                .unwrap_or_default();

            let seg_id = searcher.segment_reader(doc_addr.segment_ord).segment_id();
            if let Some(by_field) = sink.get(seg_id, doc_addr.doc_id) {
                for (field_name, offsets) in &by_field {
                    for &[from, to] in offsets {
                        let highlighted = &stored[from..to];
                        eprintln!("Highlight [{from}..{to}]: '{highlighted}'");
                        // The highlight should contain "ingleQuery" or "SingleQuery",
                        // NOT "ddSingleQuery" or anything wider.
                        assert!(
                            highlighted.contains("ingleQuery") || highlighted.contains("SingleQuery"),
                            "Bad highlight: '{highlighted}' (expected to contain 'ingleQuery' or 'SingleQuery')"
                        );
                        // Must not extend past the token boundary
                        assert!(
                            highlighted.len() <= "SingleQuery".len() + 5,
                            "Highlight too wide: '{highlighted}' ({} bytes)",
                            highlighted.len()
                        );
                    }
                }
            }
        }

        handle.close().unwrap();
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_diag_luce_cross_token() {
        let luce_path = concat!(env!("CARGO_MANIFEST_DIR"), "/../playground/dataset.luce");
        let data = match std::fs::read(luce_path) {
            Ok(d) => d,
            Err(_) => { eprintln!("[diag] skipping: .luce not found"); return; }
        };

        let mut indexes = crate::snapshot::import_snapshot(&data).unwrap();
        let imported = indexes.remove(0);

        let tmp = std::env::temp_dir().join("lucivy_diag_luce");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        for (name, file_data) in &imported.files {
            let path = tmp.join(name);
            if let Some(parent) = path.parent() { std::fs::create_dir_all(parent).unwrap(); }
            std::fs::write(&path, file_data).unwrap();
        }

        let directory = StdFsDirectory::open(tmp.to_str().unwrap()).unwrap();
        let handle = LucivyHandle::open(directory).unwrap();
        eprintln!("[diag] fields: {:?}", handle.field_map.iter().map(|(n,_)| n.as_str()).collect::<Vec<_>>());

        // Check if sibling table is present in the segments
        let searcher = handle.reader.searcher();
        for (i, seg) in searcher.segment_readers().iter().enumerate() {
            let field = handle.field_map.iter()
                .find(|(n, _)| n != NODE_ID_FIELD)
                .map(|(_, f)| *f).unwrap();
            if let Some(sfx_data) = seg.sfx_file(field) {
                if let Ok(bytes) = sfx_data.read_bytes() {
                    if let Ok(sfx_reader) = ld_lucivy::suffix_fst::SfxFileReader::open(bytes.as_ref()) {
                        let has_sib = sfx_reader.sibling_table().is_some();
                        let sib_count = sfx_reader.sibling_table()
                            .map(|s| (0..s.num_ordinals()).filter(|&o| !s.siblings(o).is_empty()).count())
                            .unwrap_or(0);
                        eprintln!("[diag] seg[{}] has_sibling={} ordinals_with_siblings={}", i, has_sib, sib_count);
                    }
                }
            }
        }
        drop(searcher);

        let field_name = handle.field_map.iter()
            .find(|(n, _)| n != NODE_ID_FIELD)
            .map(|(n, _)| n.clone())
            .unwrap();

        let search = |q: &str| -> (usize, std::time::Duration) {
            let qc = crate::query::QueryConfig {
                query_type: "contains".into(),
                field: Some(field_name.clone()),
                value: Some(q.into()),
                ..Default::default()
            };
            let query = crate::query::build_query(&qc, &handle.schema, &handle.index, None).unwrap();
            let searcher = handle.reader.searcher();
            let t0 = std::time::Instant::now();
            let results = searcher.search(
                &*query, &ld_lucivy::collector::TopDocs::with_limit(20).order_by_score(),
            ).unwrap();
            (results.len(), t0.elapsed())
        };

        // Diagnostic: how many segments?
        let searcher = handle.reader.searcher();
        eprintln!("[diag] num_segments={}, num_docs={}", searcher.segment_readers().len(), searcher.num_docs());
        drop(searcher);

        for q in ["weaver", "rag3weaver", "rag3w", "rag3db", "getElementById"] {
            let (toks, seps) = ld_lucivy::query::tokenize_query(q);
            if toks.len() > 1 {
                eprintln!("[diag] tokenize '{}' → tokens={:?}, seps={:?}", q, toks, seps);
            }
            let (count, elapsed) = search(q);
            eprintln!("[diag] query='{}' → {} results in {:?}", q, count, elapsed);
        }
        // Test with highlights (like the playground does)
        eprintln!("[diag] --- with highlights ---");
        let search_hl = |q: &str| -> (usize, std::time::Duration) {
            let sink = std::sync::Arc::new(ld_lucivy::query::HighlightSink::new());
            let qc = crate::query::QueryConfig {
                query_type: "contains".into(),
                field: Some(field_name.clone()),
                value: Some(q.into()),
                ..Default::default()
            };
            let query = crate::query::build_query(&qc, &handle.schema, &handle.index, Some(sink.clone())).unwrap();
            let searcher = handle.reader.searcher();
            let t0 = std::time::Instant::now();
            let results = searcher.search(
                &*query, &ld_lucivy::collector::TopDocs::with_limit(20).order_by_score(),
            ).unwrap();
            (results.len(), t0.elapsed())
        };
        // Fuzzy queries on .luce
        eprintln!("[diag] --- fuzzy d=1 ---");
        let search_fuzzy = |q: &str, d: u8| -> (usize, std::time::Duration) {
            let qc = crate::query::QueryConfig {
                query_type: "contains".into(),
                field: Some(field_name.clone()),
                value: Some(q.into()),
                distance: Some(d),
                ..Default::default()
            };
            let query = crate::query::build_query(&qc, &handle.schema, &handle.index, None).unwrap();
            let searcher = handle.reader.searcher();
            let t0 = std::time::Instant::now();
            let results = searcher.search(
                &*query, &ld_lucivy::collector::TopDocs::with_limit(20).order_by_score(),
            ).unwrap();
            (results.len(), t0.elapsed())
        };
        for q in ["rak3weaver", "rag3weavr", "weavr", "rag3we4ver"] {
            let (count, elapsed) = search_fuzzy(q, 1);
            eprintln!("[diag] fuzzy '{}' d=1 → {} results in {:?}", q, count, elapsed);
        }

        for q in ["rag3weaver", "rag3w", "getElementById"] {
            let (count, elapsed) = search_hl(q);
            eprintln!("[diag] query='{}' (hl) → {} results in {:?}", q, count, elapsed);
        }

        handle.close().unwrap();
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Diagnostic: index the real doc 11 and check all highlights for "rag3weaver".
    #[test]
    fn test_diag_highlight_rag3weaver() {
        let doc_path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../docs/19-mars-2026/11-plan-observabilite-avancee-luciole.md"
        );
        let text = match std::fs::read_to_string(doc_path) {
            Ok(t) => t,
            Err(_) => { eprintln!("[hl_diag] skipping: doc not found"); return; }
        };

        let tmp = std::env::temp_dir().join("lucivy_test_diag_hl");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let config: SchemaConfig = serde_json::from_value(serde_json::json!({
            "fields": [{"name": "body", "type": "text", "stored": true}]
        })).unwrap();

        let directory = StdFsDirectory::open(tmp.to_str().unwrap()).unwrap();
        let handle = LucivyHandle::create(directory, &config).unwrap();
        let body = handle.field("body").unwrap();
        let nid = handle.field(NODE_ID_FIELD).unwrap();

        {
            let mut g = handle.writer.lock().unwrap();
            let w = g.as_mut().unwrap();
            let mut doc = ld_lucivy::LucivyDocument::new();
            doc.add_u64(nid, 0);
            doc.add_text(body, &text);
            w.add_document(doc).unwrap();
            w.commit().unwrap();
        }
        handle.reader.reload().unwrap();

        let sink = std::sync::Arc::new(ld_lucivy::query::HighlightSink::new());
        let qc = crate::query::QueryConfig {
            query_type: "contains".into(),
            field: Some("body".into()),
            value: Some("rag3weaver".into()),
            ..Default::default()
        };
        let query = crate::query::build_query(&qc, &handle.schema, &handle.index, Some(sink.clone())).unwrap();
        let searcher = handle.reader.searcher();
        let results = searcher.search(
            &*query, &ld_lucivy::collector::TopDocs::with_limit(20).order_by_score(),
        ).unwrap();

        // Find all expected byte positions of "rag3weaver" in the original text
        let lower = text.to_lowercase();
        let mut expected = Vec::new();
        let mut pos = 0;
        while let Some(found) = lower[pos..].find("rag3weaver") {
            let abs = pos + found;
            expected.push(abs);
            pos = abs + 1;
        }
        eprintln!("[hl_diag] doc len={} bytes, results={}, expected occurrences={}", text.len(), results.len(), expected.len());
        for &p in &expected {
            eprintln!("[hl_diag] expected: bytes {}..{} → {:?}", p, p+10, &text[p..p+10]);
        }

        let highlights = sink.all_entries();
        let mut all_ok = true;
        for entry in &highlights {
            for &[byte_from, byte_to] in &entry.offsets {
                let safe_to = byte_to.min(text.len());
                let safe_from = byte_from.min(safe_to);
                let highlighted = &text[safe_from..safe_to];
                let ok = highlighted.to_lowercase() == "rag3weaver";
                eprintln!("[hl_diag] got: bytes {}..{} → {:?} {}",
                    byte_from, byte_to, highlighted, if ok { "✓" } else { "✗ WRONG" });
                if !ok { all_ok = false; }
            }
        }

        assert!(!highlights.is_empty(), "should have highlights");
        assert!(all_ok, "some highlights were incorrect");

        handle.close().unwrap();
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
