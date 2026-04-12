//! lucivy-emscripten — C FFI for ld-lucivy, compiled to WASM via emscripten.
//!
//! All operations go through ShardedHandle (unified handle, even for 1 shard).
//! Threading: emscripten pthreads + PROXY_TO_PTHREAD.

mod opfs;

// ── SharedArrayBuffer log ring buffer ─────────────────────────────

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU32, Ordering};

const RING_SIZE: usize = 65536;
const RING_HEADER: usize = 8;

#[repr(C, align(4))]
struct LogRing(UnsafeCell<[u8; RING_SIZE]>);
unsafe impl Sync for LogRing {}

static LOG_RING: LogRing = LogRing(UnsafeCell::new([0u8; RING_SIZE]));
static RING_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn ring_ptr() -> *mut u8 {
    LOG_RING.0.get() as *mut u8
}

fn ring_write_pos() -> &'static AtomicU32 {
    unsafe { &*(ring_ptr() as *const AtomicU32) }
}

fn ring_wrap_count() -> &'static AtomicU32 {
    unsafe { &*(ring_ptr().add(4) as *const AtomicU32) }
}

fn ring_write(msg: &str) {
    let bytes = msg.as_bytes();
    let entry_size = 2 + bytes.len();
    if entry_size > RING_SIZE - RING_HEADER {
        return;
    }

    let _lock = RING_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let mut pos = ring_write_pos().load(Ordering::Relaxed) as usize;
    if pos < RING_HEADER {
        pos = RING_HEADER;
    }

    if pos + entry_size > RING_SIZE {
        pos = RING_HEADER;
        ring_wrap_count().fetch_add(1, Ordering::Release);
    }

    unsafe {
        let p = ring_ptr();
        let len = bytes.len() as u16;
        *p.add(pos) = (len & 0xFF) as u8;
        *p.add(pos + 1) = (len >> 8) as u8;
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p.add(pos + 2), bytes.len());
    }

    ring_write_pos().store((pos + entry_size) as u32, Ordering::Release);
}

#[no_mangle]
pub extern "C" fn lucivy_log_ring_ptr() -> *const u8 {
    ring_ptr()
}

#[no_mangle]
pub extern "C" fn lucivy_log_ring_size() -> u32 {
    RING_SIZE as u32
}

// ── Global log buffer ─────────────────────────────────────────────

static LOG_BUF: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

fn rlog(msg: &str) {
    ring_write(msg);
    if let Ok(mut buf) = LOG_BUF.lock() {
        buf.push(msg.to_string());
    }
    eprintln!("{msg}");
}

macro_rules! rlog {
    ($($arg:tt)*) => { rlog(&format!($($arg)*)) };
}

#[no_mangle]
pub extern "C" fn __main_argc_argv(_argc: i32, _argv: *const *const c_char) -> i32 {
    std::env::set_var("LUCIVY_SCHEDULER_THREADS", "4");
    rlog!("[lucivy-wasm] main() started, default scheduler_threads=4");
    0
}

#[no_mangle]
pub unsafe extern "C" fn lucivy_configure(
    scheduler_threads: u32,
    thread_pool_size: u32,
) {
    if scheduler_threads > 0 {
        std::env::set_var(
            "LUCIVY_SCHEDULER_THREADS",
            scheduler_threads.to_string(),
        );
        let reserved_for_others = 3;
        let total_needed = scheduler_threads + reserved_for_others;
        if thread_pool_size > 0 && total_needed >= thread_pool_size {
            rlog!(
                "[lucivy-wasm] WARNING: scheduler_threads={scheduler_threads} + \
                 {reserved_for_others} reserved = {total_needed} threads needed, \
                 but PTHREAD_POOL_SIZE={thread_pool_size}. \
                 Reduce scheduler_threads or increase PTHREAD_POOL_SIZE."
            );
        }
        rlog!(
            "[lucivy-wasm] configured: scheduler_threads={scheduler_threads}, \
             pool_size={thread_pool_size}"
        );
    }
}

use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::sync::Arc;

#[no_mangle]
pub unsafe extern "C" fn lucivy_read_logs() -> *const c_char {
    let logs: Vec<String> = LOG_BUF.lock().map(|mut b| b.drain(..).collect()).unwrap_or_default();
    let json = serde_json::to_string(&logs).unwrap_or_else(|_| "[]".into());
    return_str(json)
}

use ld_lucivy::query::HighlightSink;
use ld_lucivy::schema::{FieldType, Value};
use ld_lucivy::LucivyDocument;

use lucivy_core::handle::NODE_ID_FIELD;
use lucivy_core::query;
use lucivy_core::snapshot;
use lucivy_core::sharded_handle::{ShardedHandle, ShardStorage, RamShardStorage, ShardedSearchResult};

// ── Context ──────────────────────────────────────────────────────────────

struct LucivyContext {
    handle: ShardedHandle,
    text_fields: Vec<String>,
    index_path: String,
}

// ── Thread-local return buffer ───────────────────────────────────────────

thread_local! {
    static RETURN_BUF: std::cell::RefCell<CString> =
        std::cell::RefCell::new(CString::new("").unwrap());
}

fn return_str(s: String) -> *const c_char {
    let c = CString::new(s).unwrap_or_else(|_| CString::new("").unwrap());
    RETURN_BUF.with(|buf| {
        *buf.borrow_mut() = c;
        buf.borrow().as_ptr()
    })
}

fn return_error(msg: &str) -> *const c_char {
    let json = serde_json::json!({"error": msg}).to_string();
    return_str(json)
}

unsafe fn str_from_ptr<'a>(ptr: *const c_char) -> &'a str {
    if ptr.is_null() { return ""; }
    CStr::from_ptr(ptr).to_str().unwrap_or("")
}

// ── Lifecycle ────────────────────────────────────────────────────────────

#[no_mangle]
pub unsafe extern "C" fn lucivy_create(
    path: *const c_char,
    config_json: *const c_char,
) -> *mut LucivyContext {
    let path = str_from_ptr(path);
    let config_json = str_from_ptr(config_json);

    let config: query::SchemaConfig = match serde_json::from_str(config_json) {
        Ok(c) => c,
        Err(_) => {
            let fields: Vec<query::FieldDef> = match serde_json::from_str(config_json) {
                Ok(f) => f,
                Err(_) => return std::ptr::null_mut(),
            };
            query::SchemaConfig {
                fields,
                tokenizer: None,
                ..Default::default()
            }
        }
    };

    let text_fields = extract_text_fields(&config);
    let storage = RamShardStorage::new();
    let handle = match ShardedHandle::create_with_storage(Box::new(storage), &config) {
        Ok(h) => h,
        Err(e) => {
            rlog!("[create] error: {e}");
            return std::ptr::null_mut();
        }
    };

    Box::into_raw(Box::new(LucivyContext {
        handle,
        text_fields,
        index_path: path.to_string(),
    }))
}

/// Open: two-phase (begin → import files → finish).
/// For non-sharded snapshots, files go into shard_0.
#[no_mangle]
pub unsafe extern "C" fn lucivy_open_begin(path: *const c_char) -> *mut LucivyContext {
    let path = str_from_ptr(path);
    Box::into_raw(Box::new(OpenContext {
        storage: RamShardStorage::new(),
        path: path.to_string(),
    })) as *mut LucivyContext
}

struct OpenContext {
    storage: RamShardStorage,
    path: String,
}

#[no_mangle]
pub unsafe extern "C" fn lucivy_import_file(
    ctx: *mut LucivyContext,
    name: *const c_char,
    data: *const u8,
    len: usize,
) {
    if ctx.is_null() || name.is_null() || data.is_null() { return; }
    let open_ctx = &mut *(ctx as *mut OpenContext);
    let name = str_from_ptr(name);
    let bytes = std::slice::from_raw_parts(data, len);

    // Root files go to storage root, shard files go to shard_0.
    if name == "_shard_config.json" || name == "_shard_stats.bin" {
        let _ = open_ctx.storage.write_root_file(name, bytes);
    } else {
        open_ctx.storage.import_shard_file(0, name, bytes.to_vec());
    }
}

#[no_mangle]
pub unsafe extern "C" fn lucivy_open_finish(ctx: *mut LucivyContext) -> *mut LucivyContext {
    if ctx.is_null() { return std::ptr::null_mut(); }
    let open_ctx = Box::from_raw(ctx as *mut OpenContext);

    // If no _shard_config.json was imported, generate one from _config.json.
    if !open_ctx.storage.root_file_exists("_shard_config.json") {
        // Read _config.json from shard_0's files to get the schema.
        match open_ctx.storage.read_root_file("_shard_config.json") {
            Ok(_) => {} // already exists
            Err(_) => {
                // Try reading _config.json from shard_0 to generate shard config.
                if let Ok(shard_handle) = open_ctx.storage.open_shard_handle(0) {
                    if let Some(config) = &shard_handle.config {
                        let mut shard_config = config.clone();
                        shard_config.shards = Some(1);
                        if let Ok(json) = serde_json::to_string(&shard_config) {
                            let _ = open_ctx.storage.write_root_file(
                                "_shard_config.json",
                                json.as_bytes(),
                            );
                        }
                    }
                    drop(shard_handle);
                }
            }
        }
    }

    let handle = match ShardedHandle::open_with_storage(Box::new(open_ctx.storage)) {
        Ok(h) => h,
        Err(e) => {
            rlog!("[open_finish] error: {e}");
            return std::ptr::null_mut();
        }
    };

    let text_fields = extract_text_fields(&handle.config);

    Box::into_raw(Box::new(LucivyContext {
        handle,
        text_fields,
        index_path: open_ctx.path,
    }))
}

#[no_mangle]
pub unsafe extern "C" fn lucivy_close(ctx: *mut LucivyContext) -> *const c_char {
    if ctx.is_null() { return return_error("null context"); }
    let ctx = &*ctx;
    match ctx.handle.close() {
        Ok(()) => return_str("ok".into()),
        Err(e) => return_error(&e),
    }
}

#[no_mangle]
pub unsafe extern "C" fn lucivy_destroy(ctx: *mut LucivyContext) {
    if !ctx.is_null() {
        drop(Box::from_raw(ctx));
    }
}

// ── Document operations ──────────────────────────────────────────────────

#[no_mangle]
pub unsafe extern "C" fn lucivy_add(
    ctx: *mut LucivyContext,
    doc_id: u32,
    fields_json: *const c_char,
) -> *const c_char {
    let ctx = &*ctx;
    let fields_json = str_from_ptr(fields_json);

    let fields: HashMap<String, serde_json::Value> = match serde_json::from_str(fields_json) {
        Ok(f) => f,
        Err(e) => return return_error(&format!("invalid fields JSON: {e}")),
    };

    let mut doc = LucivyDocument::new();
    let nid_field = match ctx.handle.field(NODE_ID_FIELD) {
        Some(f) => f,
        None => return return_error("no _node_id field"),
    };
    doc.add_u64(nid_field, doc_id as u64);

    for (key, value) in &fields {
        if let Err(e) = add_field_value(&ctx.handle, &mut doc, key, value) {
            return return_error(&e);
        }
    }

    match ctx.handle.add_document(doc, doc_id as u64) {
        Ok(()) => return_str("ok".into()),
        Err(e) => return_error(&e),
    }
}

#[no_mangle]
pub unsafe extern "C" fn lucivy_add_many(
    ctx: *mut LucivyContext,
    docs_json: *const c_char,
) -> *const c_char {
    let ctx = &*ctx;
    let docs_json = str_from_ptr(docs_json);

    let docs: Vec<serde_json::Value> = match serde_json::from_str(docs_json) {
        Ok(d) => d,
        Err(e) => return return_error(&format!("invalid docs JSON: {e}")),
    };

    let nid_field = match ctx.handle.field(NODE_ID_FIELD) {
        Some(f) => f,
        None => return return_error("no _node_id field"),
    };

    for item in &docs {
        let obj = match item.as_object() {
            Some(o) => o,
            None => return return_error("each doc must be an object"),
        };

        let doc_id = match obj.get("docId").or_else(|| obj.get("doc_id")).and_then(|v| v.as_u64()) {
            Some(id) => id,
            None => return return_error("each doc must have a 'docId' (number) key"),
        };

        let mut doc = LucivyDocument::new();
        doc.add_u64(nid_field, doc_id);

        for (key, value) in obj {
            if key == "docId" || key == "doc_id" { continue; }
            if let Err(e) = add_field_value(&ctx.handle, &mut doc, key, value) {
                return return_error(&e);
            }
        }

        if let Err(e) = ctx.handle.add_document(doc, doc_id) {
            return return_error(&e);
        }
    }

    return_str("ok".into())
}

#[no_mangle]
pub unsafe extern "C" fn lucivy_remove(ctx: *mut LucivyContext, doc_id: u32) -> *const c_char {
    let ctx = &*ctx;
    match ctx.handle.delete_by_node_id(doc_id as u64) {
        Ok(()) => return_str("ok".into()),
        Err(e) => return_error(&e),
    }
}

#[no_mangle]
pub unsafe extern "C" fn lucivy_update(
    ctx: *mut LucivyContext,
    doc_id: u32,
    fields_json: *const c_char,
) -> *const c_char {
    lucivy_remove(ctx, doc_id);
    lucivy_add(ctx, doc_id, fields_json)
}

// ── Transaction ──────────────────────────────────────────────────────────

static COMMIT_STATUS: AtomicU32 = AtomicU32::new(0);

#[no_mangle]
pub extern "C" fn lucivy_commit_status_ptr() -> *const u32 {
    &COMMIT_STATUS as *const AtomicU32 as *const u32
}

#[no_mangle]
pub unsafe extern "C" fn lucivy_commit_async(ctx: *mut LucivyContext) -> i32 {
    if COMMIT_STATUS.load(Ordering::Relaxed) == 1 {
        ring_write("[commit] already running!");
        return -1;
    }
    COMMIT_STATUS.store(1, Ordering::Release);

    let ctx_ptr = ctx as usize;
    std::thread::spawn(move || {
        let ctx = &*(ctx_ptr as *const LucivyContext);
        ring_write("[commit-thread] started");

        match ctx.handle.commit() {
            Ok(()) => {
                ring_write("[commit-thread] done OK");
                COMMIT_STATUS.store(2, Ordering::Release);
            }
            Err(e) => {
                ring_write(&format!("[commit-thread] error: {e}"));
                COMMIT_STATUS.store(3, Ordering::Release);
            }
        }
    });

    ring_write("[commit] thread spawned");
    0
}

#[no_mangle]
pub extern "C" fn lucivy_commit_finish() -> *const c_char {
    let status = COMMIT_STATUS.load(Ordering::Acquire);
    COMMIT_STATUS.store(0, Ordering::Release);
    if status == 2 {
        return_str("ok".into())
    } else {
        return_error("commit failed (check ring buffer logs)")
    }
}

#[no_mangle]
pub unsafe extern "C" fn lucivy_drain_merges(ctx: *mut LucivyContext) -> *const c_char {
    let ctx = &*ctx;
    // Commit merges all segments within each shard.
    match ctx.handle.commit() {
        Ok(()) => return_str("ok".into()),
        Err(e) => return_error(&format!("drain_merges: {e}")),
    }
}

// ── Snapshot (LUCE format) ────────────────────────────────────────────────

thread_local! {
    static SNAPSHOT_BUF: std::cell::RefCell<Vec<u8>> = std::cell::RefCell::new(Vec::new());
}

#[no_mangle]
pub unsafe extern "C" fn lucivy_export_snapshot(
    _ctx: *mut LucivyContext,
    _out_len: *mut u32,
) -> *const u8 {
    // TODO: export from ShardedHandle's RamShardStorage
    // For now, not supported in unified handle mode.
    return_error("export_snapshot not yet supported with unified handle");
    std::ptr::null()
}

#[no_mangle]
pub unsafe extern "C" fn lucivy_import_snapshot(
    data: *const u8,
    len: usize,
    path: *const c_char,
) -> *mut LucivyContext {
    if data.is_null() || len == 0 { return std::ptr::null_mut(); }
    let path = str_from_ptr(path);
    let blob = std::slice::from_raw_parts(data, len);

    let mut snap = match snapshot::import_snapshot(blob) {
        Ok(s) => s,
        Err(e) => {
            rlog!("[import_snapshot] error: {e}");
            return std::ptr::null_mut();
        }
    };
    if snap.indexes.is_empty() { return std::ptr::null_mut(); }

    let storage = RamShardStorage::new();

    if snap.is_sharded {
        // Sharded snapshot: import root files + per-shard files.
        for (name, data) in &snap.root_files {
            let _ = storage.write_root_file(name, data);
        }
        for index in &snap.indexes {
            // Parse shard_id from path (e.g. "shard_0" → 0).
            let shard_id = index.path.strip_prefix("shard_")
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(0);
            for (name, data) in &index.files {
                storage.import_shard_file(shard_id, name, data.clone());
            }
        }
    } else {
        // Non-sharded: wrap in shard_0, generate _shard_config.json.
        let imported = &snap.indexes[0];
        for (name, data) in &imported.files {
            storage.import_shard_file(0, name, data.clone());
        }
        // Generate shard config from _config.json.
        if let Some((_, config_data)) = imported.files.iter().find(|(n, _)| n == "_config.json") {
            if let Ok(mut config) = serde_json::from_slice::<query::SchemaConfig>(config_data) {
                config.shards = Some(1);
                if let Ok(json) = serde_json::to_string(&config) {
                    let _ = storage.write_root_file("_shard_config.json", json.as_bytes());
                }
            }
        }
    }

    let handle = match ShardedHandle::open_with_storage(Box::new(storage)) {
        Ok(h) => h,
        Err(e) => {
            rlog!("[import_snapshot] open error: {e}");
            return std::ptr::null_mut();
        }
    };

    let text_fields = extract_text_fields(&handle.config);

    Box::into_raw(Box::new(LucivyContext {
        handle,
        text_fields,
        index_path: path.to_string(),
    }))
}

// ── Search ───────────────────────────────────────────────────────────────

#[no_mangle]
pub unsafe extern "C" fn lucivy_search(
    ctx: *mut LucivyContext,
    query_json: *const c_char,
    limit: u32,
    highlights: i32,
    include_fields: i32,
) -> *const c_char {
    let ctx = &*ctx;
    let query_json = str_from_ptr(query_json);
    let want_highlights = highlights != 0;
    let want_fields = include_fields != 0;

    let query_config = match parse_query(ctx, query_json) {
        Ok(q) => q,
        Err(e) => return return_error(&e),
    };

    let highlight_sink = if want_highlights {
        Some(Arc::new(HighlightSink::new()))
    } else {
        None
    };

    rlog!("[search] {} shards, {} docs", ctx.handle.num_shards(), ctx.handle.num_docs());

    let results = match ctx.handle.search(&query_config, limit as usize, highlight_sink.clone()) {
        Ok(r) => r,
        Err(e) => return return_error(&e),
    };

    let json_results = match collect_sharded_results(&ctx.handle, &results, highlight_sink.as_deref(), want_fields) {
        Ok(r) => r,
        Err(e) => return return_error(&e),
    };

    match serde_json::to_string(&json_results) {
        Ok(s) => return_str(s),
        Err(e) => return_error(&format!("serialize error: {e}")),
    }
}

#[no_mangle]
pub unsafe extern "C" fn lucivy_search_filtered(
    ctx: *mut LucivyContext,
    query_json: *const c_char,
    limit: u32,
    allowed_ids: *const u32,
    ids_len: usize,
    highlights: i32,
    include_fields: i32,
) -> *const c_char {
    let ctx = &*ctx;
    let query_json = str_from_ptr(query_json);
    let want_highlights = highlights != 0;
    let want_fields = include_fields != 0;

    let id_slice = if allowed_ids.is_null() || ids_len == 0 {
        &[]
    } else {
        std::slice::from_raw_parts(allowed_ids, ids_len)
    };
    let id_set: HashSet<u64> = id_slice.iter().map(|&id| id as u64).collect();

    let query_config = match parse_query(ctx, query_json) {
        Ok(q) => q,
        Err(e) => return return_error(&e),
    };

    let highlight_sink = if want_highlights {
        Some(Arc::new(HighlightSink::new()))
    } else {
        None
    };

    let results = match ctx.handle.search_filtered(&query_config, limit as usize, highlight_sink.clone(), id_set) {
        Ok(r) => r,
        Err(e) => return return_error(&e),
    };

    let json_results = match collect_sharded_results(&ctx.handle, &results, highlight_sink.as_deref(), want_fields) {
        Ok(r) => r,
        Err(e) => return return_error(&e),
    };

    match serde_json::to_string(&json_results) {
        Ok(s) => return_str(s),
        Err(e) => return_error(&format!("serialize error: {e}")),
    }
}

// ── Info ─────────────────────────────────────────────────────────────────

#[no_mangle]
pub unsafe extern "C" fn lucivy_num_docs(ctx: *mut LucivyContext) -> u32 {
    let ctx = &*ctx;
    ctx.handle.num_docs() as u32
}

#[no_mangle]
pub unsafe extern "C" fn lucivy_schema_json(ctx: *mut LucivyContext) -> *const c_char {
    let ctx = &*ctx;
    return_str(serde_json::to_string(&ctx.handle.config).unwrap_or_default())
}

// ── Internal helpers ─────────────────────────────────────────────────────

fn add_field_value(
    handle: &ShardedHandle,
    doc: &mut LucivyDocument,
    field_name: &str,
    value: &serde_json::Value,
) -> Result<(), String> {
    let field = handle.field(field_name)
        .ok_or_else(|| format!("unknown field: {field_name}"))?;
    let field_entry = handle.schema.get_field_entry(field);

    match field_entry.field_type() {
        FieldType::Str(_) => {
            let text = value.as_str()
                .ok_or_else(|| format!("expected string for {field_name}"))?;
            doc.add_text(field, text);
        }
        FieldType::U64(_) => {
            let v = value.as_u64()
                .ok_or_else(|| format!("expected u64 for {field_name}"))?;
            doc.add_u64(field, v);
        }
        FieldType::I64(_) => {
            let v = value.as_i64()
                .ok_or_else(|| format!("expected i64 for {field_name}"))?;
            doc.add_i64(field, v);
        }
        FieldType::F64(_) => {
            let v = value.as_f64()
                .ok_or_else(|| format!("expected f64 for {field_name}"))?;
            doc.add_f64(field, v);
        }
        _ => return Err(format!("unsupported field type for {field_name}")),
    }
    Ok(())
}

fn extract_text_fields(config: &query::SchemaConfig) -> Vec<String> {
    config.fields.iter()
        .filter(|f| f.field_type == "text")
        .map(|f| f.name.clone())
        .collect()
}

fn parse_query(ctx: &LucivyContext, query_json: &str) -> Result<query::QueryConfig, String> {
    let value: serde_json::Value = serde_json::from_str(query_json)
        .map_err(|e| format!("invalid query JSON: {e}"))?;

    match &value {
        serde_json::Value::String(s) => {
            if ctx.text_fields.is_empty() {
                return Err("no text fields for string query".into());
            }
            Ok(build_contains_split_multi_field(s, &ctx.text_fields, None))
        }
        serde_json::Value::Object(_) => {
            let config: query::QueryConfig = serde_json::from_value(value)
                .map_err(|e| format!("invalid query object: {e}"))?;
            Ok(config)
        }
        _ => Err("query must be a JSON string or object".into()),
    }
}

// ── Contains split helpers ───────────────────────────────────────────────

fn build_contains_split_multi_field(value: &str, text_fields: &[String], distance: Option<u8>) -> query::QueryConfig {
    if text_fields.len() == 1 {
        return query::QueryConfig {
            query_type: "contains_split".into(),
            field: Some(text_fields[0].clone()),
            value: Some(value.to_string()),
            distance,
            ..Default::default()
        };
    }
    let words: Vec<&str> = value.split_whitespace()
        .filter(|w| w.chars().any(|c| c.is_alphanumeric()))
        .collect();
    let word_queries: Vec<query::QueryConfig> = words.iter()
        .map(|word| {
            let field_queries: Vec<query::QueryConfig> = text_fields.iter()
                .map(|f| query::QueryConfig {
                    query_type: "contains".into(),
                    field: Some(f.clone()),
                    value: Some(word.to_string()),
                    distance,
                    ..Default::default()
                })
                .collect();
            query::QueryConfig {
                query_type: "boolean".into(),
                should: Some(field_queries),
                ..Default::default()
            }
        })
        .collect();
    if word_queries.len() == 1 {
        word_queries.into_iter().next().unwrap()
    } else {
        query::QueryConfig {
            query_type: "boolean".into(),
            should: Some(word_queries),
            ..Default::default()
        }
    }
}

// ── Search result collection ─────────────────────────────────────────────

#[derive(serde::Serialize)]
struct SearchResultJson {
    #[serde(rename = "docId")]
    doc_id: u32,
    score: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    highlights: Option<HashMap<String, Vec<[u32; 2]>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fields: Option<HashMap<String, serde_json::Value>>,
}

fn collect_sharded_results(
    handle: &ShardedHandle,
    results: &[ShardedSearchResult],
    highlight_sink: Option<&HighlightSink>,
    include_fields: bool,
) -> Result<Vec<SearchResultJson>, String> {
    let mut json_results = Vec::with_capacity(results.len());

    for r in results {
        let shard = handle.shard(r.shard_id)
            .ok_or_else(|| format!("shard {} not found", r.shard_id))?;
        let searcher = shard.reader.searcher();
        let doc: LucivyDocument = searcher.doc(r.doc_address)
            .map_err(|e| e.to_string())?;

        let nid_field = handle.schema.get_field(NODE_ID_FIELD)
            .map_err(|_| "no _node_id field")?;
        let doc_id = doc.get_first(nid_field)
            .and_then(|v| v.as_value().as_u64())
            .unwrap_or(0);

        let highlights = highlight_sink.and_then(|sink| {
            let seg_id = searcher.segment_reader(r.doc_address.segment_ord).segment_id();
            let by_field = sink.get(seg_id, r.doc_address.doc_id)?;
            let map: HashMap<String, Vec<[u32; 2]>> = by_field.into_iter()
                .map(|(name, offsets)| {
                    let ranges: Vec<[u32; 2]> = offsets.into_iter()
                        .map(|[s, e]| [s as u32, e as u32])
                        .collect();
                    (name, ranges)
                })
                .collect();
            if map.is_empty() { None } else { Some(map) }
        });

        let fields = if include_fields {
            let mut map = HashMap::new();
            for (field, value) in doc.field_values() {
                let name = handle.schema.get_field_name(field);
                if name == NODE_ID_FIELD { continue; }
                let rv = value.as_value();
                let json_val = if let Some(s) = rv.as_str() {
                    serde_json::Value::String(String::from(s))
                } else if let Some(n) = rv.as_u64() {
                    serde_json::json!(n)
                } else if let Some(n) = rv.as_i64() {
                    serde_json::json!(n)
                } else if let Some(n) = rv.as_f64() {
                    serde_json::json!(n)
                } else {
                    continue;
                };
                map.insert(name.to_string(), json_val);
            }
            if map.is_empty() { None } else { Some(map) }
        } else {
            None
        };

        json_results.push(SearchResultJson {
            doc_id: doc_id as u32,
            score: r.score,
            highlights,
            fields,
        });
    }

    Ok(json_results)
}

// ── Base64 encoding ──────────────────────────────────────────────────────

fn base64_encode(data: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut result = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        result.push(CHARS[((n >> 18) & 0x3F) as usize] as char);
        result.push(CHARS[((n >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            result.push(CHARS[((n >> 6) & 0x3F) as usize] as char);
        } else {
            result.push('=');
        }
        if chunk.len() > 2 {
            result.push(CHARS[(n & 0x3F) as usize] as char);
        } else {
            result.push('=');
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields_one() -> Vec<String> { vec!["content".into()] }
    fn fields_two() -> Vec<String> { vec!["title".into(), "body".into()] }

    #[test]
    fn build_contains_split_propagates_distance_single_field() {
        let q = build_contains_split_multi_field("hello world", &fields_one(), Some(3));
        assert_eq!(q.query_type, "contains_split");
        assert_eq!(q.distance, Some(3));
    }

    #[test]
    fn build_contains_split_propagates_distance_multi_field() {
        let q = build_contains_split_multi_field("hello", &fields_two(), Some(2));
        assert_eq!(q.query_type, "boolean");
        for sub in q.should.as_ref().unwrap() {
            assert_eq!(sub.query_type, "contains");
            assert_eq!(sub.distance, Some(2));
        }
    }

    #[test]
    fn build_contains_split_none_distance_stays_none() {
        let q = build_contains_split_multi_field("hello world", &fields_one(), None);
        assert_eq!(q.query_type, "contains_split");
        assert_eq!(q.distance, None);
    }

    #[test]
    fn build_contains_split_single_field_delegates_to_core() {
        let q = build_contains_split_multi_field("hello world", &fields_one(), Some(3));
        assert_eq!(q.query_type, "contains_split");
        assert_eq!(q.field.as_deref(), Some("content"));
        assert_eq!(q.distance, Some(3));
    }
}
