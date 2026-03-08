//! Index handle management.
//!
//! Each LucivyHandle holds an Index, an IndexWriter, and an IndexReader.
//!
//! Every "text" field gets a triple-field layout:
//!   - `{name}` : tokenized (stemmed if stemmer configured, else lowercase)
//!   - `{name}._raw` : lowercased only (for term/fuzzy/regex/contains queries — precision)
//!   - `{name}._ngram` : trigrams (for fast substring candidate generation in contains queries)
//! The routing is transparent — users always reference the base field name.

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

/// Suffix appended to text fields for the non-stemmed counterpart.
pub const RAW_SUFFIX: &str = "._raw";

/// Suffix appended to text fields for the n-gram (trigram) counterpart.
pub const NGRAM_SUFFIX: &str = "._ngram";

/// Tokenizer name for stemmed fields.
const STEMMED_TOKENIZER: &str = "stemmed";

/// Tokenizer name for n-gram (trigram) fields.
const NGRAM_TOKENIZER: &str = "ngram";

/// Opaque handle shared by all bindings.
pub struct LucivyHandle {
    pub index: Index,
    pub writer: Mutex<IndexWriter>,
    pub reader: IndexReader,
    pub schema: Schema,
    /// Maps field names (including internal `._raw` names) to Field objects.
    pub field_map: Vec<(String, Field)>,
    /// Maps user field names to their `._raw` counterpart names.
    /// Always populated for "text" fields.
    pub raw_field_pairs: Vec<(String, String)>,
    /// Maps user field names to their `._ngram` counterpart names.
    /// Always populated for "text" fields.
    pub ngram_field_pairs: Vec<(String, String)>,
    /// Original schema config, available for bindings that need field metadata on open().
    pub config: Option<SchemaConfig>,
    /// True if there are uncommitted changes (add/remove/update without commit).
    pub has_uncommitted: AtomicBool,
}

/// Default writer heap size (50MB).
const WRITER_HEAP_SIZE: usize = 50_000_000;

/// Config file stored alongside the index for reopening.
const CONFIG_FILE: &str = "_config.json";

/// Create an IndexWriter with a thread count appropriate for the target.
/// On WASM, limit to 1 thread to avoid exhausting the emscripten pthread pool.
fn create_writer(index: &Index) -> Result<IndexWriter, String> {
    #[cfg(target_arch = "wasm32")]
    {
        index
            .writer_with_num_threads(1, WRITER_HEAP_SIZE)
            .map_err(|e| format!("cannot create writer: {e}"))
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        index
            .writer(WRITER_HEAP_SIZE)
            .map_err(|e| format!("cannot create writer: {e}"))
    }
}

impl LucivyHandle {
    /// Create a new index with the given directory and schema config.
    pub fn create(dir: impl Directory, config: &SchemaConfig) -> Result<Self, String> {
        let (schema, field_map, raw_field_pairs, ngram_field_pairs) = build_schema(config)?;

        // Persist config BEFORE creating the index, so it bypasses ManagedDirectory's GC.
        // ManagedDirectory.atomic_write registers files as "managed" and the GC deletes them
        // on commit because they are not referenced by any segment. Writing directly on the
        // underlying Directory avoids this.
        let config_json =
            serde_json::to_string(config).map_err(|e| format!("cannot serialize config: {e}"))?;
        dir.atomic_write(Path::new(CONFIG_FILE), config_json.as_bytes())
            .map_err(|e| format!("cannot write config: {e}"))?;

        let index = Index::create(dir, schema.clone(), IndexSettings::default())
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
            writer: Mutex::new(writer),
            reader,
            schema,
            field_map,
            raw_field_pairs,
            ngram_field_pairs,
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

        // Use the pre-read config to re-register tokenizers and rebuild field pairs.
        let (config, raw_field_pairs, ngram_field_pairs) = match config_bytes {
            Some(config_data) => {
                match serde_json::from_slice::<SchemaConfig>(&config_data) {
                    Ok(config) => {
                        configure_tokenizers(&index, &config);
                        let text_fields: Vec<_> = config
                            .fields
                            .iter()
                            .filter(|f| f.field_type == "text")
                            .collect();
                        let string_fields: Vec<_> = config
                            .fields
                            .iter()
                            .filter(|f| f.field_type == "string")
                            .collect();
                        let raw: Vec<_> = text_fields
                            .iter()
                            .map(|f| (f.name.clone(), format!("{}{RAW_SUFFIX}", f.name)))
                            .collect();
                        let ngram: Vec<_> = text_fields
                            .iter()
                            .chain(string_fields.iter())
                            .map(|f| (f.name.clone(), format!("{}{NGRAM_SUFFIX}", f.name)))
                            .collect();
                        (Some(config), raw, ngram)
                    }
                    Err(_) => (None, Vec::new(), Vec::new()),
                }
            }
            None => (None, Vec::new(), Vec::new()),
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
            writer: Mutex::new(writer),
            reader,
            schema,
            field_map,
            raw_field_pairs,
            ngram_field_pairs,
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
) -> Result<(Schema, Vec<(String, Field)>, Vec<(String, String)>, Vec<(String, String)>), String> {
    let mut builder = Schema::builder();
    let mut field_map = Vec::new();
    let mut raw_field_pairs = Vec::new();
    let mut ngram_field_pairs = Vec::new();
    let has_stemmer = config.stemmer.is_some();

    // Auto-add _node_id as u64 FAST + INDEXED + STORED field.
    // STORED is required so that extract_node_id() can read it back from documents.
    let node_id_field = builder.add_u64_field(NODE_ID_FIELD, FAST | INDEXED | STORED);
    field_map.push((NODE_ID_FIELD.to_string(), node_id_field));

    for field_def in &config.fields {
        match field_def.field_type.as_str() {
            "text" => {
                // Main field: stemmed tokenizer if stemmer configured, else "default" (lowercase).
                let main_tokenizer = if has_stemmer { STEMMED_TOKENIZER } else { "default" };
                let indexing = TextFieldIndexing::default()
                    .set_tokenizer(main_tokenizer)
                    .set_index_option(IndexRecordOption::WithFreqsAndPositionsAndOffsets);
                let mut opts = TextOptions::default().set_indexing_options(indexing);
                if field_def.stored.unwrap_or(true) {
                    opts = opts.set_stored();
                }
                let field = builder.add_text_field(&field_def.name, opts);
                field_map.push((field_def.name.clone(), field));

                // Raw counterpart: "default" tokenizer (lowercase only), NOT stored.
                // Used by term/fuzzy/regex/contains queries for precision matching.
                let raw_indexing = TextFieldIndexing::default()
                    .set_tokenizer("default")
                    .set_index_option(IndexRecordOption::WithFreqsAndPositionsAndOffsets);
                let raw_opts = TextOptions::default().set_indexing_options(raw_indexing);
                let raw_name = format!("{}{RAW_SUFFIX}", field_def.name);
                let raw_field = builder.add_text_field(&raw_name, raw_opts);
                field_map.push((raw_name.clone(), raw_field));
                raw_field_pairs.push((field_def.name.clone(), raw_name));

                // N-gram counterpart: trigrams for fast substring candidate generation.
                // Uses IndexRecordOption::Basic (doc IDs only — no positions/offsets needed).
                let ngram_indexing = TextFieldIndexing::default()
                    .set_tokenizer(NGRAM_TOKENIZER)
                    .set_index_option(IndexRecordOption::Basic);
                let ngram_opts = TextOptions::default().set_indexing_options(ngram_indexing);
                let ngram_name = format!("{}{NGRAM_SUFFIX}", field_def.name);
                let ngram_field = builder.add_text_field(&ngram_name, ngram_opts);
                field_map.push((ngram_name.clone(), ngram_field));
                ngram_field_pairs.push((field_def.name.clone(), ngram_name));
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

                // Ngram counterpart for substring matching (NgramContainsQuery).
                let ngram_indexing = TextFieldIndexing::default()
                    .set_tokenizer(NGRAM_TOKENIZER)
                    .set_index_option(IndexRecordOption::Basic);
                let ngram_opts = TextOptions::default().set_indexing_options(ngram_indexing);
                let ngram_name = format!("{}{NGRAM_SUFFIX}", field_def.name);
                let ngram_field = builder.add_text_field(&ngram_name, ngram_opts);
                field_map.push((ngram_name.clone(), ngram_field));
                ngram_field_pairs.push((field_def.name.clone(), ngram_name));
            }
            other => return Err(format!("unknown field type: {other}")),
        }
    }

    Ok((builder.build(), field_map, raw_field_pairs, ngram_field_pairs))
}

pub fn configure_tokenizers(index: &Index, config: &SchemaConfig) {
    use ld_lucivy::tokenizer::{AsciiFoldingFilter, LowerCaser, SimpleTokenizer, TextAnalyzer};

    use crate::tokenizer::NgramFilter;

    // N-gram tokenizer: always registered (used by ._ngram fields for contains queries).
    // AsciiFoldingFilter normalizes diacritics (ç→c, é→e) so that ngram candidates
    // are not missed when query/data differ only by accents.
    let ngram_tokenizer = TextAnalyzer::builder(SimpleTokenizer::default())
        .filter(LowerCaser)
        .filter(AsciiFoldingFilter)
        .filter(NgramFilter)
        .build();
    index.tokenizers().register(NGRAM_TOKENIZER, ngram_tokenizer);

    // Stemmer: only if requested.
    if let Some(ref stemmer_lang) = config.stemmer {
        use ld_lucivy::tokenizer::Stemmer;

        let lang = match stemmer_lang.as_str() {
            "english" => ld_lucivy::tokenizer::Language::English,
            "french" => ld_lucivy::tokenizer::Language::French,
            "german" => ld_lucivy::tokenizer::Language::German,
            "spanish" => ld_lucivy::tokenizer::Language::Spanish,
            "italian" => ld_lucivy::tokenizer::Language::Italian,
            "portuguese" => ld_lucivy::tokenizer::Language::Portuguese,
            "dutch" => ld_lucivy::tokenizer::Language::Dutch,
            "russian" => ld_lucivy::tokenizer::Language::Russian,
            _ => return,
        };

        let tokenizer = TextAnalyzer::builder(SimpleTokenizer::default())
            .filter(LowerCaser)
            .filter(Stemmer::new(lang))
            .build();
        index.tokenizers().register(STEMMED_TOKENIZER, tokenizer);
    }
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
    fn test_string_filter_field_contains() {
        let tmp = std::env::temp_dir().join("lucivy_test_string_filter_contains");
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

        // Verify ngram pairs include "tag"
        assert!(
            handle.ngram_field_pairs.iter().any(|(user, _)| user == "tag"),
            "ngram_field_pairs should contain tag: {:?}", handle.ngram_field_pairs
        );

        // Add documents
        let body_field = handle.field("body").unwrap();
        let tag_field = handle.field("tag").unwrap();
        let nid_field = handle.field(NODE_ID_FIELD).unwrap();

        {
            let mut writer = handle.writer.lock().unwrap();
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
                // Auto-duplicate to ngram fields
                for (user, ngram_name) in &handle.ngram_field_pairs {
                    if user == "body" {
                        if let Some(f) = handle.field(ngram_name) { doc.add_text(f, body); }
                    }
                    if user == "tag" {
                        if let Some(f) = handle.field(ngram_name) { doc.add_text(f, tag); }
                    }
                }
                // Also raw field for body
                for (user, raw_name) in &handle.raw_field_pairs {
                    if user == "body" {
                        if let Some(f) = handle.field(raw_name) { doc.add_text(f, body); }
                    }
                }
                writer.add_document(doc).unwrap();
            }
            writer.commit().unwrap();
        }
        handle.reader.reload().unwrap();

        // Search: body contains "programming" + filter tag contains "ystem"
        let query_json = r#"{
            "type": "contains",
            "field": "body",
            "value": "programming",
            "filters": [{"field": "tag", "op": "contains", "value": "ystem"}]
        }"#;
        let query_config: crate::query::QueryConfig = serde_json::from_str(query_json).unwrap();
        let query = crate::query::build_query(
            &query_config,
            &handle.schema,
            &handle.index,
            &handle.raw_field_pairs,
            &handle.ngram_field_pairs,
            None,
        ).unwrap();

        let searcher = handle.reader.searcher();
        let collector = ld_lucivy::collector::TopDocs::with_limit(10).order_by_score();
        let results = searcher.search(&*query, &collector).unwrap();

        println!("Results for contains 'ystem' filter: {:?}", results);
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
            let mut writer = handle.writer.lock().unwrap();
            let mut doc = ld_lucivy::LucivyDocument::new();
            doc.add_u64(nid_field, 42);
            doc.add_text(path_field, "src/main.rs");
            doc.add_text(content_field, "fn main() { println!(\"hello\"); }");
            // Add to raw/ngram fields
            for (user, raw_name) in &handle.raw_field_pairs {
                if let Some(f) = handle.field(raw_name) {
                    if user == "path" { doc.add_text(f, "src/main.rs"); }
                    if user == "content" { doc.add_text(f, "fn main() { println!(\"hello\"); }"); }
                }
            }
            for (user, ngram_name) in &handle.ngram_field_pairs {
                if let Some(f) = handle.field(ngram_name) {
                    if user == "path" { doc.add_text(f, "src/main.rs"); }
                    if user == "content" { doc.add_text(f, "fn main() { println!(\"hello\"); }"); }
                }
            }
            writer.add_document(doc).unwrap();
            writer.commit().unwrap();
        }
        handle.reader.reload().unwrap();

        // Search
        let query_config: crate::query::QueryConfig = serde_json::from_str(
            r#"{"type": "contains", "field": "content", "value": "main"}"#
        ).unwrap();
        let query = crate::query::build_query(
            &query_config, &handle.schema, &handle.index,
            &handle.raw_field_pairs, &handle.ngram_field_pairs, None,
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

        // Check stored text fields (skip internal _raw/_ngram)
        let mut fields: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        for (field, value) in doc.field_values() {
            let name = handle.schema.get_field_name(field);
            if name == NODE_ID_FIELD || name.ends_with(RAW_SUFFIX) || name.ends_with(NGRAM_SUFFIX) {
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
}
