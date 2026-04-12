/**
 * OPFS bridge for lucivy-emscripten.
 *
 * Implements the extern "C" functions declared in opfs.rs.
 * Each function starts an async OPFS operation and signals completion
 * via an AtomicU32 in shared WASM memory.
 *
 * Signal values: 0=pending, 1=ok, 2=error
 *
 * Include via emcc --js-library opfs-bridge.js
 */

mergeInto(LibraryManager.library, {

  // ── Availability check ──────────────────────────────────────────────

  js_opfs_available: function() {
    return (typeof navigator !== 'undefined' &&
            navigator.storage &&
            typeof navigator.storage.getDirectory === 'function') ? 1 : 0;
  },

  // ── Write ───────────────────────────────────────────────────────────

  js_opfs_write: function(signalPtr, pathPtr, pathLen, dataPtr, dataLen) {
    var path = UTF8ArrayToString(HEAPU8, pathPtr, pathLen);
    var data = HEAPU8.slice(dataPtr, dataPtr + dataLen);

    (async function() {
      try {
        var root = await navigator.storage.getDirectory();
        // Create subdirectories if needed (e.g. "shard_0/meta.json")
        var parts = path.split('/');
        var dir = root;
        for (var i = 0; i < parts.length - 1; i++) {
          dir = await dir.getDirectoryHandle(parts[i], { create: true });
        }
        var fileName = parts[parts.length - 1];
        var fileHandle = await dir.getFileHandle(fileName, { create: true });
        var writable = await fileHandle.createWritable();
        await writable.write(data);
        await writable.close();
        Atomics.store(HEAP32, signalPtr >> 2, 1); // SIGNAL_OK
      } catch (e) {
        console.error('[opfs-bridge] write error:', path, e);
        Atomics.store(HEAP32, signalPtr >> 2, 2); // SIGNAL_ERROR
      }
    })();
  },

  // ── Delete ──────────────────────────────────────────────────────────

  js_opfs_delete: function(signalPtr, pathPtr, pathLen) {
    var path = UTF8ArrayToString(HEAPU8, pathPtr, pathLen);

    (async function() {
      try {
        var root = await navigator.storage.getDirectory();
        var parts = path.split('/');
        var dir = root;
        for (var i = 0; i < parts.length - 1; i++) {
          dir = await dir.getDirectoryHandle(parts[i]);
        }
        var fileName = parts[parts.length - 1];
        await dir.removeEntry(fileName);
        Atomics.store(HEAP32, signalPtr >> 2, 1);
      } catch (e) {
        // File not found is OK for delete
        Atomics.store(HEAP32, signalPtr >> 2, 1);
      }
    })();
  },

  // ── List directory ──────────────────────────────────────────────────

  js_opfs_list: function(signalPtr, pathPtr, pathLen) {
    var path = UTF8ArrayToString(HEAPU8, pathPtr, pathLen);

    (async function() {
      try {
        var root = await navigator.storage.getDirectory();
        var dir = root;
        if (path && path !== '/') {
          var parts = path.split('/').filter(Boolean);
          for (var i = 0; i < parts.length; i++) {
            dir = await dir.getDirectoryHandle(parts[i]);
          }
        }
        // Store result in a global for Rust to retrieve
        var entries = [];
        for await (var [name, handle] of dir.entries()) {
          entries.push({ name: name, kind: handle.kind });
        }
        Module._opfs_list_result = entries;
        Atomics.store(HEAP32, signalPtr >> 2, 1);
      } catch (e) {
        Module._opfs_list_result = [];
        Atomics.store(HEAP32, signalPtr >> 2, 2);
      }
    })();
  },

  // ── Read file ───────────────────────────────────────────────────────

  js_opfs_read: function(signalPtr, pathPtr, pathLen, resultPtrOut, resultLenOut) {
    var path = UTF8ArrayToString(HEAPU8, pathPtr, pathLen);

    (async function() {
      try {
        var root = await navigator.storage.getDirectory();
        var parts = path.split('/');
        var dir = root;
        for (var i = 0; i < parts.length - 1; i++) {
          dir = await dir.getDirectoryHandle(parts[i]);
        }
        var fileName = parts[parts.length - 1];
        var fileHandle = await dir.getFileHandle(fileName);
        var file = await fileHandle.getFile();
        var buffer = await file.arrayBuffer();
        var bytes = new Uint8Array(buffer);

        // Allocate WASM memory and copy data
        var wasmPtr = _malloc(bytes.length);
        HEAPU8.set(bytes, wasmPtr);

        // Write result pointer and length
        HEAPU32[resultPtrOut >> 2] = wasmPtr;
        HEAPU32[resultLenOut >> 2] = bytes.length;

        Atomics.store(HEAP32, signalPtr >> 2, 1);
      } catch (e) {
        HEAPU32[resultPtrOut >> 2] = 0;
        HEAPU32[resultLenOut >> 2] = 0;
        Atomics.store(HEAP32, signalPtr >> 2, 2);
      }
    })();
  },

});
