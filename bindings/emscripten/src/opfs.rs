//! OPFS bridge — async file persistence via JS Promises + SignalFuture.
//!
//! Rust → JS FFI (fire the async op) → JS resolves → sets AtomicU32 signal
//! → luciole AsyncActor polls → SignalFuture completes.
//!
//! Write flow:
//!   opfs_write_async(path, data) → SignalFuture
//!   JS: open sync access handle → write → close → signal OK
//!
//! Read flow:
//!   opfs_read_async(path) → SignalDataFuture
//!   JS: open file → read → copy to WASM heap → signal OK
//!
//! All functions return futures — use with luciole::AsyncScope.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use luciole::async_executor::{SignalFuture, SIGNAL_OK, SIGNAL_PENDING};

// ── JS FFI declarations ────────────────────────────────────────────────
//
// These are implemented in opfs-bridge.js (included via --js-library).
// Each function starts an async OPFS operation and returns immediately.
// When the operation completes, JS sets the signal AtomicU32 to SIGNAL_OK.

extern "C" {
    /// Write data to OPFS. Sets signal to 1 on success, 2 on error.
    fn js_opfs_write(
        signal_ptr: *const u32,
        path_ptr: *const u8,
        path_len: u32,
        data_ptr: *const u8,
        data_len: u32,
    );

    /// Delete a file from OPFS. Sets signal to 1 on success, 2 on error.
    fn js_opfs_delete(
        signal_ptr: *const u32,
        path_ptr: *const u8,
        path_len: u32,
    );

    /// List files in an OPFS directory. Sets signal to 1 when done.
    /// Result is written to a global JS buffer, retrieved via js_opfs_list_result.
    fn js_opfs_list(
        signal_ptr: *const u32,
        path_ptr: *const u8,
        path_len: u32,
    );

    /// Read a file from OPFS into WASM memory.
    /// JS allocates WASM memory via _malloc, writes data, stores ptr+len
    /// at result_ptr_out and result_len_out.
    fn js_opfs_read(
        signal_ptr: *const u32,
        path_ptr: *const u8,
        path_len: u32,
        result_ptr_out: *mut u32,
        result_len_out: *mut u32,
    );

    /// Check if OPFS is available (returns 1 if yes, 0 if no).
    fn js_opfs_available() -> u32;
}

// ── Public async API ───────────────────────────────────────────────────

/// Check if OPFS is available in this browser context.
pub fn is_available() -> bool {
    unsafe { js_opfs_available() == 1 }
}

/// Write a file to OPFS asynchronously. Returns a future.
pub fn write_async(path: &str, data: &[u8]) -> SignalFuture {
    let signal = Arc::new(AtomicU32::new(SIGNAL_PENDING));
    let ptr = Arc::as_ptr(&signal) as *const u32;
    unsafe {
        js_opfs_write(
            ptr,
            path.as_ptr(),
            path.len() as u32,
            data.as_ptr(),
            data.len() as u32,
        );
    }
    // Keep signal alive — the Arc prevents deallocation while JS holds the pointer.
    // This is safe because the SignalFuture holds the Arc.
    SignalFuture::from_signal(signal)
}

/// Delete a file from OPFS asynchronously. Returns a future.
pub fn delete_async(path: &str) -> SignalFuture {
    let signal = Arc::new(AtomicU32::new(SIGNAL_PENDING));
    let ptr = Arc::as_ptr(&signal) as *const u32;
    unsafe {
        js_opfs_delete(ptr, path.as_ptr(), path.len() as u32);
    }
    SignalFuture::from_signal(signal)
}

/// Persist a set of files to OPFS. Fire-and-forget via AsyncScope.
///
/// Usage:
/// ```ignore
/// let scope = AsyncScope::new(Priority::Idle);
/// persist_files(&scope, "shard_0", &[("meta.json", data1), ("seg.data", data2)]);
/// ```
pub fn persist_files(
    scope: &luciole::AsyncScope,
    base_path: &str,
    files: &[(String, Vec<u8>)],
) {
    for (name, data) in files {
        let path = format!("{base_path}/{name}");
        let data = data.clone();
        scope.spawn_detached(async move {
            let fut = write_async(&path, &data);
            let _ = fut.await;
        });
    }
}
