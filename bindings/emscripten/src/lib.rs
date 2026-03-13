//! lucivy-emscripten — C FFI for ld-lucivy, compiled to WASM via emscripten.
//!
//! Provides extern "C" functions that emscripten exposes to JavaScript.
//! All threading (rayon, std::thread) works natively via emscripten pthreads.
//!
//! Built with `-sPROXY_TO_PTHREAD` so that blocking calls (commit, search)
//! don't deadlock the event loop needed for pthread coordination.

mod directory;

// ── SharedArrayBuffer log ring buffer ─────────────────────────────
// Readable directly from JS via SharedArrayBuffer — no ccall needed.
// This is critical because during a deadlock the proxy pthread is blocked
// and ccall-based log reading is impossible.
//
// Layout (64 KB):
//   [0..4]  write_pos   (AtomicU32) — next byte offset to write at
//   [4..8]  wrap_count  (AtomicU32) — incremented on each wrap-around
//   [8..]   entries, each: [u16_le len][utf8 bytes...]
//
// JS reader tracks (readPos, lastWrap) and polls every 50ms via Atomics.

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

/// Return pointer to the log ring buffer (for JS to read via SharedArrayBuffer).
#[no_mangle]
pub extern "C" fn lucivy_log_ring_ptr() -> *const u8 {
    ring_ptr()
}

/// Return size of the log ring buffer.
#[no_mangle]
pub extern "C" fn lucivy_log_ring_size() -> u32 {
    RING_SIZE as u32
}

// ── Global log buffer (readable from JS via lucivy_read_logs) ───────────

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

/// Dummy main required by emscripten's PROXY_TO_PTHREAD mode.
/// The actual work is done via exported C functions called from JS.
#[no_mangle]
pub extern "C" fn __main_argc_argv(_argc: i32, _argv: *const *const c_char) -> i32 {
    // Default: limit scheduler to 4 threads (PTHREAD_POOL_SIZE=8 in build.sh).
    // Can be overridden by calling lucivy_configure() before first index op.
    std::env::set_var("LUCIVY_SCHEDULER_THREADS", "4");
    rlog!("[lucivy-wasm] main() started, default scheduler_threads=4");
    0
}

/// Configure the scheduler before any index operation.
/// Must be called before `lucivy_create` / `lucivy_open_begin`.
///
/// - `scheduler_threads`: number of threads for the actor scheduler.
/// - `thread_pool_size`: total emscripten pthread pool size (PTHREAD_POOL_SIZE
///   from build.sh). Used only to warn if the scheduler would exhaust the pool,
///   leaving no room for commit threads.
///
/// If `scheduler_threads` is 0, the scheduler auto-detects (available_parallelism).
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
        // The scheduler uses `scheduler_threads` persistent threads.
        // Commit, warm-gc, and debug-logger each need one more.
        // If the total leaves no headroom, warn loudly.
        let reserved_for_others = 3; // commit + warm-gc + debug-logger
        let total_needed = scheduler_threads + reserved_for_others;
        if thread_pool_size > 0 && total_needed >= thread_pool_size {
            rlog!(
                "[lucivy-wasm] WARNING: scheduler_threads={scheduler_threads} + \
                 {reserved_for_others} reserved = {total_needed} threads needed, \
                 but PTHREAD_POOL_SIZE={thread_pool_size}. \
                 Commit will not be able to spawn its processing thread. \
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

/// Read and drain accumulated Rust-side logs. Returns JSON array of strings.
#[no_mangle]
pub unsafe extern "C" fn lucivy_read_logs() -> *const c_char {
    let logs: Vec<String> = LOG_BUF.lock().map(|mut b| b.drain(..).collect()).unwrap_or_default();
    let json = serde_json::to_string(&logs).unwrap_or_else(|_| "[]".into());
    return_str(json)
}

use ld_lucivy::collector::{FilterCollector, TopDocs};
use ld_lucivy::query::HighlightSink;
use ld_lucivy::schema::{FieldType, Value};
use ld_lucivy::{DocAddress, LucivyDocument, Searcher};

use lucivy_core::handle::{LucivyHandle, NGRAM_SUFFIX, NODE_ID_FIELD, RAW_SUFFIX};
use lucivy_core::query;
use lucivy_core::snapshot;

use crate::directory::MemoryDirectory;

// ── Context ────────────────────────────────────────────────────────────────

struct LucivyContext {
    handle: LucivyHandle,
    text_fields: Vec<String>,
    directory: MemoryDirectory,
    index_path: String,
}


// ── Thread-local return buffer ─────────────────────────────────────────────

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

// ── Lifecycle ──────────────────────────────────────────────────────────────

/// Create a new index.
/// Returns opaque pointer to LucivyContext, or null on error.
/// `error_out` receives error message if non-null and creation fails.
#[no_mangle]
pub unsafe extern "C" fn lucivy_create(
    path: *const c_char,
    fields_json: *const c_char,
    stemmer: *const c_char,
) -> *mut LucivyContext {
    let path = str_from_ptr(path);
    let fields_json = str_from_ptr(fields_json);
    let stemmer = str_from_ptr(stemmer);

    let fields: Vec<query::FieldDef> = match serde_json::from_str(fields_json) {
        Ok(f) => f,
        Err(_) => return std::ptr::null_mut(),
    };

    let config = query::SchemaConfig {
        fields,
        tokenizer: None,
        stemmer: if stemmer.is_empty() { None } else { Some(stemmer.to_string()) },
    };

    let directory = MemoryDirectory::new();
    let handle = match LucivyHandle::create(directory.clone(), &config) {
        Ok(h) => h,
        Err(_) => return std::ptr::null_mut(),
    };

    let text_fields = extract_text_fields(&config);

    Box::into_raw(Box::new(LucivyContext {
        handle,
        text_fields,
        directory,
        index_path: path.to_string(),

    }))
}

/// Open an existing index. Files must be imported first via `lucivy_import_file`.
/// 1. Call `lucivy_open_begin(path)` to get a context with empty directory
/// 2. Call `lucivy_import_file(ctx, name, data, len)` for each file
/// 3. Call `lucivy_open_finish(ctx)` to finalize
#[no_mangle]
pub unsafe extern "C" fn lucivy_open_begin(path: *const c_char) -> *mut LucivyContext {
    let path = str_from_ptr(path);
    // Return a context with an empty directory + null handle.
    // We use a temporary struct that will be completed by open_finish.
    let directory = MemoryDirectory::new();
    // We need a placeholder LucivyHandle — but we can't create one without files.
    // Use a two-phase approach: store directory in a separate struct.
    Box::into_raw(Box::new(OpenContext {
        directory,
        path: path.to_string(),
    })) as *mut LucivyContext
}

struct OpenContext {
    directory: MemoryDirectory,
    path: String,
}

/// Import a file into the directory being opened.
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
    open_ctx.directory.import_file(name, bytes.to_vec());
}

/// Finish opening the index after importing files. Returns the final context.
/// On error returns null and the open context is freed.
#[no_mangle]
pub unsafe extern "C" fn lucivy_open_finish(ctx: *mut LucivyContext) -> *mut LucivyContext {
    if ctx.is_null() { return std::ptr::null_mut(); }
    let open_ctx = Box::from_raw(ctx as *mut OpenContext);

    let handle = match LucivyHandle::open(open_ctx.directory.clone()) {
        Ok(h) => h,
        Err(_) => return std::ptr::null_mut(),
    };

    let text_fields = match &handle.config {
        Some(config) => extract_text_fields(config),
        None => Vec::new(),
    };

    Box::into_raw(Box::new(LucivyContext {
        handle,
        text_fields,
        directory: open_ctx.directory,
        index_path: open_ctx.path,

    }))
}

/// Destroy an index context and free memory.
#[no_mangle]
pub unsafe extern "C" fn lucivy_destroy(ctx: *mut LucivyContext) {
    if !ctx.is_null() {
        drop(Box::from_raw(ctx));
    }
}

// ── Document operations ────────────────────────────────────────────────────

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
        if let Err(e) = add_field_value(ctx, &mut doc, key, value) {
            return return_error(&e);
        }
    }

    let writer = match ctx.handle.writer.lock() {
        Ok(w) => w,
        Err(_) => return return_error("writer lock poisoned"),
    };
    let result = match writer.add_document(doc) {
        Ok(_) => {
            ctx.handle.mark_uncommitted();
            "ok".into()
        }
        Err(e) => {
            return return_error(&e.to_string());
        }
    };
    drop(writer);
    return_str(result)
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

    let writer = match ctx.handle.writer.lock() {
        Ok(w) => w,
        Err(_) => return return_error("writer lock poisoned"),
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
            if let Err(e) = add_field_value(ctx, &mut doc, key, value) {
                return return_error(&e);
            }
        }

        if let Err(e) = writer.add_document(doc) {
            return return_error(&e.to_string());
        }
    }

    ctx.handle.mark_uncommitted();
    return_str("ok".into())
}

#[no_mangle]
pub unsafe extern "C" fn lucivy_remove(ctx: *mut LucivyContext, doc_id: u32) -> *const c_char {
    let ctx = &*ctx;
    let field = match ctx.handle.field(NODE_ID_FIELD) {
        Some(f) => f,
        None => return return_error("no _node_id field"),
    };
    let term = ld_lucivy::schema::Term::from_field_u64(field, doc_id as u64);
    let writer = match ctx.handle.writer.lock() {
        Ok(w) => w,
        Err(_) => return return_error("writer lock poisoned"),
    };
    writer.delete_term(term);
    ctx.handle.mark_uncommitted();
    return_str("ok".into())
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

// ── Transaction ────────────────────────────────────────────────────────────

// ── Commit on dedicated pthread (bypasses ASYNCIFY entirely) ───────────────
//
// ASYNCIFY cannot handle the deep blocking call stack inside writer.commit().
// Instead we spawn a real std::thread (from the PTHREAD_POOL), do the commit
// there, and signal completion via an AtomicU32 that JS reads directly from
// the SharedArrayBuffer with Atomics.load() — zero ccall for polling.
//
// Status: 0=idle, 1=running, 2=done_ok, 3=done_error

static COMMIT_STATUS: AtomicU32 = AtomicU32::new(0);

/// Return pointer to COMMIT_STATUS so JS can poll via Atomics.load() on the SAB.
#[no_mangle]
pub extern "C" fn lucivy_commit_status_ptr() -> *const u32 {
    &COMMIT_STATUS as *const AtomicU32 as *const u32
}

/// Spawn commit on a dedicated pthread. Returns 0 on success, -1 if already running.
/// JS should poll COMMIT_STATUS via the SAB pointer until it reads 2 (ok) or 3 (error).
/// After reading 2 or 3, call lucivy_commit_finish() to get the result and reset status.
#[no_mangle]
pub unsafe extern "C" fn lucivy_commit_async(ctx: *mut LucivyContext) -> i32 {
    if COMMIT_STATUS.load(Ordering::Relaxed) == 1 {
        ring_write("[commit] already running!");
        return -1;
    }
    COMMIT_STATUS.store(1, Ordering::Release);

    let ctx_ptr = ctx as usize; // usize is Send
    std::thread::spawn(move || {
        let ctx = &mut *(ctx_ptr as *mut LucivyContext);
        ring_write("[commit-thread] started");

        ring_write("[commit-thread] acquiring writer lock...");
        let mut writer = match ctx.handle.writer.lock() {
            Ok(w) => w,
            Err(_) => {
                ring_write("[commit-thread] writer lock poisoned!");
                COMMIT_STATUS.store(3, Ordering::Release);
                return;
            }
        };
        ring_write("[commit-thread] writer lock acquired, committing...");
        if let Err(e) = writer.commit() {
            ring_write(&format!("[commit-thread] commit error: {e}"));
            drop(writer);
            COMMIT_STATUS.store(3, Ordering::Release);
            return;
        }
        drop(writer);
        ring_write("[commit-thread] committed, reloading reader...");
        if let Err(e) = ctx.handle.reader.reload() {
            ring_write(&format!("[commit-thread] reload error: {e}"));
            COMMIT_STATUS.store(3, Ordering::Release);
            return;
        }
        ctx.handle.mark_committed();
        ring_write("[commit-thread] done OK");
        COMMIT_STATUS.store(2, Ordering::Release);
    });

    ring_write("[commit] thread spawned");
    0
}

/// Read and reset commit status. Returns "ok" or {"error":"..."}.
/// Call this after COMMIT_STATUS reads 2 or 3 from JS.
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
pub unsafe extern "C" fn lucivy_rollback(ctx: *mut LucivyContext) -> *const c_char {
    let ctx = &*ctx;
    let mut writer = match ctx.handle.writer.lock() {
        Ok(w) => w,
        Err(_) => return return_error("writer lock poisoned"),
    };
    match writer.rollback() {
        Ok(_) => {
            ctx.handle.mark_committed();
            return_str("ok".into())
        }
        Err(e) => return_error(&e.to_string()),
    }
}

// ── File export (for OPFS sync) ────────────────────────────────────────────

/// Export dirty files as JSON: {"modified":[[name, base64], ...], "deleted":[name, ...]}
#[no_mangle]
pub unsafe extern "C" fn lucivy_export_dirty(ctx: *mut LucivyContext) -> *const c_char {
    let ctx = &*ctx;
    let (modified, deleted) = ctx.directory.export_dirty();

    let mod_json: Vec<serde_json::Value> = modified.iter()
        .map(|(name, data)| {
            serde_json::json!([name, base64_encode(data)])
        })
        .collect();

    let json = serde_json::json!({
        "modified": mod_json,
        "deleted": deleted,
    });
    return_str(json.to_string())
}

/// Export ALL files as JSON: [[name, base64], ...]
#[no_mangle]
pub unsafe extern "C" fn lucivy_export_all(ctx: *mut LucivyContext) -> *const c_char {
    let ctx = &*ctx;
    let files = ctx.directory.export_all();

    let json: Vec<serde_json::Value> = files.iter()
        .map(|(name, data)| serde_json::json!([name, base64_encode(data)]))
        .collect();

    return_str(serde_json::Value::Array(json).to_string())
}

// ── Snapshot (LUCE format) ──────────────────────────────────────────────────

const EXCLUDED_FILES: &[&str] = &[".lock", ".tantivy-writer.lock", ".lucivy-writer.lock", ".managed.json"];

fn collect_snapshot_files(directory: &MemoryDirectory) -> Vec<(String, Vec<u8>)> {
    directory.export_all()
        .into_iter()
        .filter(|(name, _)| !EXCLUDED_FILES.contains(&name.as_str()))
        .collect()
}

// Thread-local buffer for snapshot blobs (same pattern as RETURN_BUF for strings).
thread_local! {
    static SNAPSHOT_BUF: std::cell::RefCell<Vec<u8>> = std::cell::RefCell::new(Vec::new());
}

/// Export the index as a LUCE snapshot blob.
/// Returns a pointer to the blob data. `out_len` receives the blob length.
/// The returned pointer is valid until the next call to `lucivy_export_snapshot`.
/// Returns null on error (uncommitted changes, etc.) — check `lucivy_export_snapshot_error`.
#[no_mangle]
pub unsafe extern "C" fn lucivy_export_snapshot(
    ctx: *mut LucivyContext,
    out_len: *mut u32,
) -> *const u8 {
    if ctx.is_null() || out_len.is_null() { return std::ptr::null(); }
    let ctx = &*ctx;

    if let Err(_) = snapshot::check_committed(&ctx.handle, &ctx.index_path) {
        return_error("index has uncommitted changes — call commit before export");
        return std::ptr::null();
    }

    let files = collect_snapshot_files(&ctx.directory);
    let snap = snapshot::SnapshotIndex {
        path: &ctx.index_path,
        files,
    };
    let blob = snapshot::export_snapshot(&[snap]);

    SNAPSHOT_BUF.with(|buf| {
        *buf.borrow_mut() = blob;
        let b = buf.borrow();
        *out_len = b.len() as u32;
        b.as_ptr()
    })
}

/// Import a LUCE snapshot blob and return a new LucivyContext.
/// `data`/`len`: the LUCE blob.
/// `path`: the logical path for the imported index.
/// Returns null on error.
#[no_mangle]
pub unsafe extern "C" fn lucivy_import_snapshot(
    data: *const u8,
    len: usize,
    path: *const c_char,
) -> *mut LucivyContext {
    if data.is_null() || len == 0 { return std::ptr::null_mut(); }
    let path = str_from_ptr(path);
    let blob = std::slice::from_raw_parts(data, len);

    let mut indexes = match snapshot::import_snapshot(blob) {
        Ok(i) => i,
        Err(_) => return std::ptr::null_mut(),
    };
    if indexes.is_empty() { return std::ptr::null_mut(); }
    let imported = indexes.remove(0);

    let directory = MemoryDirectory::new();
    for (name, file_data) in &imported.files {
        directory.import_file(name, file_data.clone());
    }

    let handle = match LucivyHandle::open(directory.clone()) {
        Ok(h) => h,
        Err(_) => return std::ptr::null_mut(),
    };

    let text_fields = match &handle.config {
        Some(config) => extract_text_fields(config),
        None => Vec::new(),
    };

    Box::into_raw(Box::new(LucivyContext {
        handle,
        text_fields,
        directory,
        index_path: path.to_string(),

    }))
}

// ── Search ─────────────────────────────────────────────────────────────────

/// Search the index. Returns JSON results.
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

    let lucivy_query = match query::build_query(
        &query_config,
        &ctx.handle.schema,
        &ctx.handle.index,
        &ctx.handle.raw_field_pairs,
        &ctx.handle.ngram_field_pairs,
        highlight_sink.clone(),
    ) {
        Ok(q) => q,
        Err(e) => return return_error(&e),
    };

    let searcher = ctx.handle.reader.searcher();
    let top_docs = match execute_top_docs(&searcher, lucivy_query.as_ref(), limit) {
        Ok(d) => d,
        Err(e) => return return_error(&e),
    };
    let results = match collect_results(&searcher, &top_docs, &ctx.handle.schema, highlight_sink.as_deref(), want_fields) {
        Ok(r) => r,
        Err(e) => return return_error(&e),
    };

    match serde_json::to_string(&results) {
        Ok(s) => return_str(s),
        Err(e) => return_error(&format!("serialize error: {e}")),
    }
}

/// Search with allowed_ids filter.
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

    let lucivy_query = match query::build_query(
        &query_config,
        &ctx.handle.schema,
        &ctx.handle.index,
        &ctx.handle.raw_field_pairs,
        &ctx.handle.ngram_field_pairs,
        highlight_sink.clone(),
    ) {
        Ok(q) => q,
        Err(e) => return return_error(&e),
    };

    let searcher = ctx.handle.reader.searcher();
    let top_docs = match execute_top_docs_filtered(&searcher, lucivy_query.as_ref(), limit, id_set) {
        Ok(d) => d,
        Err(e) => return return_error(&e),
    };
    let results = match collect_results(&searcher, &top_docs, &ctx.handle.schema, highlight_sink.as_deref(), want_fields) {
        Ok(r) => r,
        Err(e) => return return_error(&e),
    };

    match serde_json::to_string(&results) {
        Ok(s) => return_str(s),
        Err(e) => return_error(&format!("serialize error: {e}")),
    }
}

// ── Info ───────────────────────────────────────────────────────────────────

#[no_mangle]
pub unsafe extern "C" fn lucivy_num_docs(ctx: *mut LucivyContext) -> u32 {
    let ctx = &*ctx;
    ctx.handle.reader.searcher().num_docs() as u32
}

#[no_mangle]
pub unsafe extern "C" fn lucivy_schema_json(ctx: *mut LucivyContext) -> *const c_char {
    let ctx = &*ctx;
    match &ctx.handle.config {
        Some(config) => return_str(serde_json::to_string(config).unwrap_or_default()),
        None => return_str(String::new()),
    }
}

// ── Internal helpers ───────────────────────────────────────────────────────

fn add_field_value(
    ctx: &LucivyContext,
    doc: &mut LucivyDocument,
    field_name: &str,
    value: &serde_json::Value,
) -> Result<(), String> {
    let field = ctx.handle.field(field_name)
        .ok_or_else(|| format!("unknown field: {field_name}"))?;
    let field_entry = ctx.handle.schema.get_field_entry(field);

    match field_entry.field_type() {
        FieldType::Str(_) => {
            let text = value.as_str()
                .ok_or_else(|| format!("expected string for {field_name}"))?;
            doc.add_text(field, text);
            auto_duplicate(ctx, doc, field_name, text);
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

fn auto_duplicate(ctx: &LucivyContext, doc: &mut LucivyDocument, field_name: &str, text: &str) {
    if let Some(raw_name) = ctx.handle.raw_field_pairs.iter()
        .find(|(user, _)| user == field_name)
        .map(|(_, raw)| raw.as_str())
    {
        if let Some(f) = ctx.handle.field(raw_name) {
            doc.add_text(f, text);
        }
    }
    if let Some(ngram_name) = ctx.handle.ngram_field_pairs.iter()
        .find(|(user, _)| user == field_name)
        .map(|(_, ngram)| ngram.as_str())
    {
        if let Some(f) = ctx.handle.field(ngram_name) {
            doc.add_text(f, text);
        }
    }
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
            let mut config: query::QueryConfig = serde_json::from_value(value)
                .map_err(|e| format!("invalid query object: {e}"))?;
            if config.query_type == "contains_split" {
                config = expand_contains_split(&config);
            } else if config.query_type == "startsWith_split" {
                config = expand_starts_with_split(&config);
            }
            Ok(config)
        }
        _ => Err("query must be a JSON string or object".into()),
    }
}

// ── Contains split helpers ─────────────────────────────────────────────────

fn build_contains_split_multi_field(value: &str, text_fields: &[String], distance: Option<u8>) -> query::QueryConfig {
    let words: Vec<&str> = value.split_whitespace().collect();
    if text_fields.len() == 1 {
        return expand_contains_split_for_field(value, &words, &text_fields[0], distance);
    }
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

fn expand_contains_split(config: &query::QueryConfig) -> query::QueryConfig {
    let value = config.value.as_deref().unwrap_or("");
    let field = config.field.as_deref().unwrap_or("");
    let words: Vec<&str> = value.split_whitespace().collect();
    expand_contains_split_for_field(value, &words, field, config.distance)
}

fn expand_contains_split_for_field(value: &str, words: &[&str], field: &str, distance: Option<u8>) -> query::QueryConfig {
    if words.len() <= 1 {
        return query::QueryConfig {
            query_type: "contains".into(),
            field: Some(field.to_string()),
            value: Some(value.to_string()),
            distance,
            ..Default::default()
        };
    }
    let should: Vec<query::QueryConfig> = words.iter()
        .map(|w| query::QueryConfig {
            query_type: "contains".into(),
            field: Some(field.to_string()),
            value: Some(w.to_string()),
            distance,
            ..Default::default()
        })
        .collect();
    query::QueryConfig {
        query_type: "boolean".into(),
        should: Some(should),
        ..Default::default()
    }
}

// ── StartsWith split helpers ──────────────────────────────────────────────

fn expand_starts_with_split(config: &query::QueryConfig) -> query::QueryConfig {
    let value = config.value.as_deref().unwrap_or("");
    let field = config.field.as_deref().unwrap_or("");
    let words: Vec<&str> = value.split_whitespace().collect();
    if words.len() <= 1 {
        return query::QueryConfig {
            query_type: "startsWith".into(),
            field: Some(field.to_string()),
            value: Some(value.to_string()),
            distance: config.distance,
            ..Default::default()
        };
    }
    let should: Vec<query::QueryConfig> = words.iter()
        .map(|w| query::QueryConfig {
            query_type: "startsWith".into(),
            field: Some(field.to_string()),
            value: Some(w.to_string()),
            distance: config.distance,
            ..Default::default()
        })
        .collect();
    query::QueryConfig {
        query_type: "boolean".into(),
        should: Some(should),
        ..Default::default()
    }
}

// ── Search helpers ─────────────────────────────────────────────────────────

fn execute_top_docs(
    searcher: &Searcher,
    query: &dyn ld_lucivy::query::Query,
    limit: u32,
) -> Result<Vec<(f32, DocAddress)>, String> {
    let collector = TopDocs::with_limit(limit as usize).order_by_score();
    searcher.search(query, &collector)
        .map_err(|e| format!("search error: {e}"))
}

fn execute_top_docs_filtered(
    searcher: &Searcher,
    query: &dyn ld_lucivy::query::Query,
    limit: u32,
    allowed_ids: HashSet<u64>,
) -> Result<Vec<(f32, DocAddress)>, String> {
    let inner = TopDocs::with_limit(limit as usize).order_by_score();
    let collector = FilterCollector::new(
        NODE_ID_FIELD.to_string(),
        move |value: u64| allowed_ids.contains(&value),
        inner,
    );
    searcher.search(query, &collector)
        .map_err(|e| format!("filtered search error: {e}"))
}

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

fn collect_results(
    searcher: &Searcher,
    top_docs: &[(f32, DocAddress)],
    schema: &ld_lucivy::schema::Schema,
    highlight_sink: Option<&HighlightSink>,
    include_fields: bool,
) -> Result<Vec<SearchResultJson>, String> {
    let nid_field = schema.get_field(NODE_ID_FIELD)
        .map_err(|_| "no _node_id field in schema".to_string())?;

    let mut results = Vec::with_capacity(top_docs.len());
    for &(score, doc_addr) in top_docs {
        let doc: LucivyDocument = searcher.doc(doc_addr)
            .map_err(|e| e.to_string())?;
        let doc_id = doc.get_first(nid_field)
            .and_then(|v| v.as_value().as_u64())
            .unwrap_or(0);

        let highlights = highlight_sink.and_then(|sink| {
            let seg_id = searcher.segment_reader(doc_addr.segment_ord).segment_id();
            let by_field = sink.get(seg_id, doc_addr.doc_id)?;
            let map: HashMap<String, Vec<[u32; 2]>> = by_field.into_iter()
                .filter(|(name, _)| !name.ends_with(RAW_SUFFIX) && !name.ends_with(NGRAM_SUFFIX))
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
                let name = schema.get_field_name(field);
                if name == NODE_ID_FIELD || name.ends_with(RAW_SUFFIX) || name.ends_with(NGRAM_SUFFIX) {
                    continue;
                }
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

        results.push(SearchResultJson {
            doc_id: doc_id as u32,
            score,
            highlights,
            fields,
        });
    }
    Ok(results)
}

// ── Base64 encoding (no extra dep) ─────────────────────────────────────────

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
        assert_eq!(q.query_type, "boolean");
        for sub in q.should.as_ref().unwrap() {
            assert_eq!(sub.query_type, "contains");
            assert_eq!(sub.distance, Some(3));
        }
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
        assert_eq!(q.query_type, "boolean");
        for sub in q.should.as_ref().unwrap() {
            assert_eq!(sub.distance, None);
        }
    }

    #[test]
    fn expand_contains_split_propagates_distance() {
        let config = query::QueryConfig {
            query_type: "contains_split".into(),
            field: Some("body".into()),
            value: Some("hello world".into()),
            distance: Some(3),
            ..Default::default()
        };
        let q = expand_contains_split(&config);
        assert_eq!(q.query_type, "boolean");
        for sub in q.should.as_ref().unwrap() {
            assert_eq!(sub.query_type, "contains");
            assert_eq!(sub.distance, Some(3));
        }
    }
}
