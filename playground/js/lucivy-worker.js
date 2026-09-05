// lucivy-worker.js — Web Worker that runs lucivy-emscripten with OPFS persistence.
//
// Threading model: the Rust side uses a global actor scheduler with persistent
// pthreads. ASYNCIFY lets blocking Rust calls (mutex, condvar) yield back to
// the event loop so emscripten can coordinate pthreads.
//
// Usage from main thread:
//   const worker = new Worker('lucivy-worker.js', { type: 'module' });
//   worker.postMessage({ type: 'init', id: 1 });
//
// Or use lucivy.js for a Promise-based API.

let Module = null;

const indexes = new Map(); // path -> ctx pointer

// ── Debug: relay worker logs to main thread ──────────────────────────────────

function wlog(...args) {
    const msg = args.map(a => typeof a === 'object' ? JSON.stringify(a) : String(a)).join(' ');
    self.postMessage({ type: 'log', msg });
    console.log(msg);
}

// ── Diag: send logs to server + poll eval commands (debug mode only) ─────────
// Debug mode is enabled when the debug server (serve.mjs) is reachable.
// In standalone mode (GitHub Pages, static server), diag is disabled automatically.

const DIAG_URL = self.location.origin;
let diagLogBatch = [];
let diagEnabled = false; // off by default, enabled after probe

function diagSendLog(line) {
    if (!diagEnabled) return;
    diagLogBatch.push(line);
    if (diagLogBatch.length >= 50) diagFlush();
}

function diagFlush() {
    if (!diagEnabled || diagLogBatch.length === 0) return;
    const batch = diagLogBatch;
    diagLogBatch = [];
    fetch(`${DIAG_URL}/log`, { method: 'POST', body: batch.join('\n') }).catch(() => {});
}

async function diagEvalPoller() {
    while (diagEnabled) {
        try {
            const resp = await fetch(`${DIAG_URL}/eval/poll`);
            const cmd = await resp.json();
            if (cmd.id && cmd.js) {
                wlog(`[eval] executing: ${cmd.js}`);
                let result = null, error = null;
                try {
                    result = String(eval(cmd.js));
                } catch (e) {
                    error = e.message;
                }
                wlog(`[eval] result: ${error ? 'ERR ' + error : result}`);
                fetch(`${DIAG_URL}/eval/result`, {
                    method: 'POST',
                    headers: { 'Content-Type': 'application/json' },
                    body: JSON.stringify({ id: cmd.id, result, error }),
                }).catch(() => {});
            }
        } catch {}
        await new Promise(r => setTimeout(r, 500));
    }
}

// Probe: check if debug server is running. Enable diag only if reachable.
(async () => {
    try {
        const resp = await fetch(`${DIAG_URL}/eval/poll`, { signal: AbortSignal.timeout(1000) });
        if (resp.ok) {
            diagEnabled = true;
            wlog('[diag] debug server detected — diag enabled');
            setInterval(diagFlush, 500);
            diagEvalPoller();
        }
    } catch {
        wlog('[diag] no debug server — standalone mode');
    }
})();

// Hook: intercept eprintln! (emscripten stderr) and send to diag server.
// emscripten routes eprintln! → Module.printErr → console.error.
// We monkey-patch console.error in the worker to capture these.
const _origConsoleError = console.error;
console.error = function(...args) {
    const msg = args.map(a => typeof a === 'object' ? JSON.stringify(a) : String(a)).join(' ');
    diagSendLog(msg);
    _origConsoleError.apply(console, args);
};

// ── Rust log poller ──────────────────────────────────────────────────────────
// Polls lucivy_read_logs() every 200ms and relays to main thread via wlog.

function startLogPoller() {
    setInterval(async () => {
        if (!Module) return;
        try {
            const ptr = await Module.ccall('lucivy_read_logs', 'number', [], [], { async: true });
            if (!ptr) return;
            const json = Module.UTF8ToString(ptr);
            const logs = JSON.parse(json);
            for (const msg of logs) {
                wlog('[rust] ' + msg);
            }
        } catch (e) { /* ignore */ }
    }, 200);
}

// ── Helpers ──────────────────────────────────────────────────────────────────

async function drainRustLogs() {
    try {
        // Try direct call first (no ASYNCIFY overhead), fall back to ccall
        let ptr;
        if (Module._lucivy_read_logs) {
            ptr = Module._lucivy_read_logs();
        } else if (Module.asm && Module.asm._lucivy_read_logs) {
            ptr = Module.asm._lucivy_read_logs();
        } else {
            // List available exports for debugging
            const exports = Module.asm ? Object.keys(Module.asm).filter(k => k.includes('lucivy')).join(', ') : 'no asm';
            wlog('[drain] lucivy_read_logs not found. exports: ' + exports);
            return;
        }
        if (ptr) {
            const json = Module.UTF8ToString(ptr);
            const logs = JSON.parse(json);
            for (const msg of logs) wlog('[rust] ' + msg);
        }
    } catch (e) { wlog('[drain] ERROR: ' + e.message); }
}

async function callStr(fn, ...args) {
    const types = args.map(a => typeof a === 'number' ? 'number' : 'string');
    const ptr = await Module.ccall(fn, 'number', types, args, { async: true });
    return Module.UTF8ToString(ptr);
}

function checkResult(res) {
    if (res.startsWith('{')) {
        const parsed = JSON.parse(res);
        if (parsed.error) throw new Error(parsed.error);
        return parsed;
    }
    if (res !== 'ok') throw new Error(res);
    return res;
}

function getCtx(path) {
    const ctx = indexes.get(path);
    if (!ctx) throw new Error(`No index open at path: ${path}`);
    return ctx;
}

// ── Base64 decode (for file export from Rust) ────────────────────────────────

function base64ToUint8Array(b64) {
    const bin = atob(b64);
    const arr = new Uint8Array(bin.length);
    for (let i = 0; i < bin.length; i++) arr[i] = bin.charCodeAt(i);
    return arr;
}

// ── OPFS helpers ─────────────────────────────────────────────────────────────

async function getOpfsDir(path) {
    const root = await navigator.storage.getDirectory();
    const parts = path.replace(/^\/+/, '').split('/').filter(Boolean);
    let dir = root;
    for (const part of parts) {
        dir = await dir.getDirectoryHandle(part, { create: true });
    }
    return dir;
}

async function readAllFiles(path) {
    const files = new Map();
    try {
        const dir = await getOpfsDir(path);
        for await (const [name, handle] of dir) {
            if (handle.kind === 'file') {
                const file = await handle.getFile();
                const buffer = await file.arrayBuffer();
                files.set(name, new Uint8Array(buffer));
            }
        }
    } catch (e) {
        // Directory doesn't exist yet — return empty map.
    }
    return files;
}

async function writeFiles(path, modified, deleted) {
    const dir = await getOpfsDir(path);

    for (const [name, data] of modified) {
        const fileHandle = await dir.getFileHandle(name, { create: true });
        const writable = await fileHandle.createWritable();
        await writable.write(data);
        await writable.close();
    }

    for (const name of deleted) {
        try {
            await dir.removeEntry(name);
        } catch (e) {
            // File may already be gone.
        }
    }
}

async function removeAllFiles(path) {
    try {
        const root = await navigator.storage.getDirectory();
        const parts = path.replace(/^\/+/, '').split('/').filter(Boolean);
        if (parts.length > 0) {
            await root.removeEntry(parts[0], { recursive: true });
        }
    } catch (e) {
        // Directory doesn't exist.
    }
}

// ── Sync dirty files from emscripten index to OPFS ──────────────────────────

async function syncDirtyToOpfs(path, ctx) {
    const dirtyJson = await callStr('lucivy_export_dirty', ctx);
    const dirty = JSON.parse(dirtyJson);

    const modified = (dirty.modified || []).map(([name, b64]) => [name, base64ToUint8Array(b64)]);
    const deleted = dirty.deleted || [];

    if (modified.length > 0 || deleted.length > 0) {
        await writeFiles(path, modified, deleted);
    }
}

// ── Message handler ──────────────────────────────────────────────────────────

self.onmessage = async (e) => {
    const { type, id, ...args } = e.data;

    try {
        if (!Module && type !== 'init') {
            throw new Error('Module not initialized — send {type: "init"} first');
        }

        let result;

        switch (type) {
            case 'init': {
                // The page's cache-buster (`?v=`) rides on this worker's URL;
                // pass it on to the engine script and, through locateFile, to
                // the wasm it fetches.
                const bust = self.location.search || '';
                const { default: createLucivy } = await import('../pkg/lucivy.js' + bust);
                Module = await createLucivy({
                    locateFile: (path, prefix) => prefix + path + (path.endsWith('.wasm') ? bust : ''),
                    // Intercept eprintln! from ALL pthreads and send to diag server.
                    printErr: function(text) {
                        diagSendLog(text);
                    },
                    // `--no-opfs`: in-memory filesystem, index lost at reload.
                    arguments: [
                        ...(args.noOpfs ? ['--no-opfs'] : []),
                        ...(args.verbose ? ['--verbose'] : []),
                        ...(args.fileCacheMb ? [`--file-cache-mb=${args.fileCacheMb}`] : []),
                        ...(args.ramIndexMaxMb ? [`--ram-index-max-mb=${args.ramIndexMaxMb}`] : []),
                        ...(args.schedulerThreads ? [`--scheduler-threads=${args.schedulerThreads}`] : []),
                        ...(args.writerThreads ? [`--writer-threads=${args.writerThreads}`] : []),
                        ...(args.maxMergedDocs ? [`--max-merged-docs=${args.maxMergedDocs}`] : []),
                        ...(args.maxBuilds ? [`--max-builds=${args.maxBuilds}`] : []),
                        ...(args.mergeConcurrency ? [`--merge-concurrency=${args.mergeConcurrency}`] : []),
                        ...(args.maxMatches !== undefined && args.maxMatches !== null ? [`--max-matches-per-segment=${args.maxMatches}`] : []),
                    ],
                });

                // Scheduler is configured to 4 threads by default in main().
                // Override with lucivy_configure() if needed.
                startLogPoller();

                // Send SharedArrayBuffer ring buffer info to main thread
                // so it can read Rust logs directly (even during deadlocks).
                try {
                    const ringPtr = await Module.ccall(
                        'lucivy_log_ring_ptr', 'number', [], [], { async: true });
                    const ringSize = await Module.ccall(
                        'lucivy_log_ring_size', 'number', [], [], { async: true });
                    if (ringPtr && ringSize && Module.HEAPU8.buffer instanceof SharedArrayBuffer) {
                        self.postMessage({
                            type: 'logRing',
                            buffer: Module.HEAPU8.buffer,
                            ringPtr,
                            ringSize,
                        });
                    }
                } catch (e) { /* log ring not available */ }

                // Get commit status pointer for SAB-based polling (zero ccall).
                try {
                    const statusPtr = await Module.ccall(
                        'lucivy_commit_status_ptr', 'number', [], [], { async: true });
                    if (statusPtr && Module.HEAPU8.buffer instanceof SharedArrayBuffer) {
                        self._commitStatusView = new Int32Array(
                            Module.HEAPU8.buffer, statusPtr, 1);
                    }
                } catch (e) { /* commit status not available */ }
                result = true;
                break;
            }

            case 'create': {
                const { path, fields, config, stemmer } = args;
                // Support both legacy (fields array) and new (full config object).
                let configJson;
                if (config) {
                    configJson = typeof config === 'string' ? config : JSON.stringify(config);
                } else {
                    configJson = typeof fields === 'string' ? fields : JSON.stringify(fields);
                }
                const ctx = await Module.ccall('lucivy_create', 'number',
                    ['string', 'string'],
                    [path, configJson], { async: true });
                if (!ctx) throw new Error('lucivy_create returned null');
                indexes.set(path, ctx);

                // Yield so the event loop can activate global scheduler pthreads
                // spawned on first use. Only matters for the very first index.
                await new Promise(r => setTimeout(r, 0));

                // Export all files to OPFS for initial persistence (best-effort).
                // Only when the binding exports it: the index already lives in
                // OPFS through WASMFS, and calling an unknown export aborts the
                // runtime under ASSERTIONS.
                if (Module._lucivy_export_all) {
                    try {
                        const allJson = await callStr('lucivy_export_all', ctx);
                        const allFiles = JSON.parse(allJson);
                        const modified = allFiles.map(([name, b64]) => [name, base64ToUint8Array(b64)]);
                        await writeFiles(path, modified, []);
                    } catch (e) {
                        console.warn('[lucivy-worker] OPFS initial sync skipped:', e.message);
                    }
                }

                result = { path, numDocs: await Module.ccall('lucivy_num_docs', 'number', ['number'], [ctx], { async: true }) };
                break;
            }

            case 'openDirect': {
                // The index already lives in OPFS through WASMFS: open it in
                // place (no JS-side file import).
                const { path } = args;
                const ctx = await Module.ccall('lucivy_open', 'number', ['string'], [path], { async: true });
                if (!ctx) throw new Error(`lucivy_open failed for ${path}`);
                indexes.set(path, ctx);
                result = { path, numDocs: await Module.ccall('lucivy_num_docs', 'number', ['number'], [ctx], { async: true }) };
                break;
            }

            case 'open': {
                const { path } = args;
                const files = await readAllFiles(path);
                if (files.size === 0) {
                    throw new Error(`No index found at OPFS path: ${path}`);
                }

                const openCtx = await Module.ccall('lucivy_open_begin', 'number', ['string'], [path], { async: true });
                if (!openCtx) throw new Error('lucivy_open_begin returned null');

                for (const [name, data] of files) {
                    const ptr = Module._malloc(data.length);
                    Module.HEAPU8.set(data, ptr);
                    const nameBytes = Module.lengthBytesUTF8(name) + 1;
                    const namePtr = Module._malloc(nameBytes);
                    Module.stringToUTF8(name, namePtr, nameBytes);
                    await Module.ccall('lucivy_import_file', null,
                        ['number', 'number', 'number', 'number'],
                        [openCtx, namePtr, ptr, data.length], { async: true });
                    Module._free(namePtr);
                    Module._free(ptr);
                }

                const ctx = await Module.ccall('lucivy_open_finish', 'number', ['number'], [openCtx], { async: true });
                if (!ctx) throw new Error('lucivy_open_finish returned null');
                indexes.set(path, ctx);

                result = { path, numDocs: await Module.ccall('lucivy_num_docs', 'number', ['number'], [ctx], { async: true }) };
                break;
            }

            case 'add': {
                const ctx = getCtx(args.path);
                const fieldsJson = typeof args.fields === 'string'
                    ? args.fields : JSON.stringify(args.fields);
                const res = await callStr('lucivy_add', ctx, args.docId, fieldsJson);
                checkResult(res);
                result = true;
                break;
            }

            case 'addMany': {
                const ctx = getCtx(args.path);
                const docsJson = typeof args.docs === 'string'
                    ? args.docs : JSON.stringify(args.docs);
                const res = await callStr('lucivy_add_many', ctx, docsJson);
                checkResult(res);
                result = true;
                break;
            }

            case 'remove': {
                const ctx = getCtx(args.path);
                const res = await callStr('lucivy_remove', ctx, args.docId);
                checkResult(res);
                result = true;
                break;
            }

            case 'update': {
                const ctx = getCtx(args.path);
                const fieldsJson = typeof args.fields === 'string'
                    ? args.fields : JSON.stringify(args.fields);
                const res = await callStr('lucivy_update', ctx, args.docId, fieldsJson);
                checkResult(res);
                result = true;
                break;
            }

            case 'commit': {
                const ctx = getCtx(args.path);

                // Commit on a pthread, status polled through the SAB: this JS
                // thread is the emscripten runtime's main thread, and blocking
                // it inside a synchronous ccall starves everything the pthreads
                // proxy to it (OPFS, thread spawn) — the 2000-doc commit of the
                // playground stalled forever that way. Falls back to the
                // synchronous call when the SAB status view is not available.
                if (self._commitStatusView) {
                    const started = await Module.ccall('lucivy_commit_async', 'number', ['number'], [ctx], { async: true });
                    if (started !== 0) throw new Error('commit already running');
                    while (Atomics.load(self._commitStatusView, 0) === 1) {
                        await new Promise(r => setTimeout(r, 20));
                    }
                    const res = await callStr('lucivy_commit_finish');
                    checkResult(res);
                } else {
                    const res = await callStr('lucivy_commit', ctx);
                    checkResult(res);
                }

                result = { numDocs: await Module.ccall('lucivy_num_docs', 'number', ['number'], [ctx], { async: true }) };
                break;
            }

            case 'rollback': {
                const ctx = getCtx(args.path);
                const res = await callStr('lucivy_rollback', ctx);
                checkResult(res);
                result = true;
                break;
            }

            case 'compact': {
                // Merge every shard down to segments of at most maxDocs
                // documents (same thread + SAB status pattern as commit).
                const ctx = getCtx(args.path);
                const started = await Module.ccall('lucivy_compact_async', 'number', ['number', 'number'], [ctx, args.maxDocs || 10000], { async: true });
                if (started !== 0) throw new Error('commit already running');
                while (Atomics.load(self._commitStatusView, 0) === 1) {
                    await new Promise(r => setTimeout(r, 50));
                }
                checkResult(await callStr('lucivy_commit_finish'));
                result = true;
                break;
            }

            case 'drainMerges': {
                // Same commit path as 'commit' (drain_merges is an alias in the
                // binding); goes through the pthread + SAB status for the same reason.
                const ctx = getCtx(args.path);
                if (self._commitStatusView) {
                    const started = await Module.ccall('lucivy_commit_async', 'number', ['number'], [ctx], { async: true });
                    if (started !== 0) throw new Error('commit already running');
                    while (Atomics.load(self._commitStatusView, 0) === 1) {
                        await new Promise(r => setTimeout(r, 20));
                    }
                    checkResult(await callStr('lucivy_commit_finish'));
                } else {
                    checkResult(await callStr('lucivy_drain_merges', ctx));
                }
                result = true;
                break;
            }

            case 'preload': {
                const ctx = getCtx(args.path);
                result = JSON.parse(await callStr('lucivy_preload', ctx));
                if (result.error) throw new Error(result.error);
                break;
            }

            case 'memoryStatus': {
                const ctx = getCtx(args.path);
                result = JSON.parse(await callStr('lucivy_memory_status', ctx));
                if (result.error) throw new Error(result.error);
                // The wasm linear memory only grows: its size is the high-water
                // mark of everything the engine ever held at once — the number
                // that says how close a build or a merge came to the 4 GB.
                result.heap_bytes = Module.HEAPU8.buffer.byteLength;
                break;
            }

            case 'search': {
                const ctx = getCtx(args.path);
                const queryJson = typeof args.query === 'string' && !args.query.startsWith('{')
                    ? JSON.stringify(args.query)
                    : (typeof args.query === 'object' ? JSON.stringify(args.query) : args.query);
                const json = await callStr('lucivy_search', ctx, queryJson, args.limit || 10, args.highlights ? 1 : 0, args.fields ? 1 : 0);
                result = JSON.parse(json);
                if (result.error) throw new Error(result.error);
                break;
            }

            case 'searchFiltered': {
                const ctx = getCtx(args.path);
                const queryJson = typeof args.query === 'string' && !args.query.startsWith('{')
                    ? JSON.stringify(args.query)
                    : (typeof args.query === 'object' ? JSON.stringify(args.query) : args.query);

                const ids = new Uint32Array(args.allowedIds);
                const idsPtr = Module._malloc(ids.byteLength);
                Module.HEAPU8.set(new Uint8Array(ids.buffer), idsPtr);

                const resPtr = await Module.ccall('lucivy_search_filtered', 'number',
                    ['number', 'string', 'number', 'number', 'number', 'number', 'number'],
                    [ctx, queryJson, args.limit || 10, idsPtr, ids.length, args.highlights ? 1 : 0, args.fields ? 1 : 0],
                    { async: true });
                const json = Module.UTF8ToString(resPtr);
                Module._free(idsPtr);

                result = JSON.parse(json);
                if (result.error) throw new Error(result.error);
                break;
            }

            case 'close': {
                indexes.delete(args.path);
                result = true;
                break;
            }

            case 'destroy': {
                indexes.delete(args.path);
                removeAllFiles(args.path).catch(() => {});
                result = true;
                break;
            }

            case 'exportSnapshot': {
                const ctx = getCtx(args.path);
                const lenPtr = Module._malloc(4);
                const dataPtr = await Module.ccall('lucivy_export_snapshot', 'number',
                    ['number', 'number'], [ctx, lenPtr], { async: true });
                if (!dataPtr) {
                    Module._free(lenPtr);
                    throw new Error('export failed — index may have uncommitted changes');
                }
                const len = Module.getValue(lenPtr, 'i32');
                Module._free(lenPtr);
                result = Module.HEAPU8.slice(dataPtr, dataPtr + len);
                break;
            }

            case 'importSnapshot': {
                const { data, path } = args;
                const dataArr = data instanceof Uint8Array ? data : new Uint8Array(data);
                const ptr = Module._malloc(dataArr.length);
                Module.HEAPU8.set(dataArr, ptr);
                const ctx = await Module.ccall('lucivy_import_snapshot', 'number',
                    ['number', 'number', 'string'], [ptr, dataArr.length, path], { async: true });
                Module._free(ptr);
                if (!ctx) throw new Error('import_snapshot failed — invalid snapshot data');
                indexes.set(path, ctx);
                result = { path, numDocs: await Module.ccall('lucivy_num_docs', 'number', ['number'], [ctx], { async: true }) };
                break;
            }

            // ── Incremental sync (LUCIDS) ───────────────────────────────
            // The three entry points below were compiled into the wasm and
            // listed in EXPORTED_FUNCTIONS from the start, but nothing above
            // them called into them, so a browser client could only ever take
            // a whole snapshot. Syncing a growing server index to a browser is
            // the case this exists for, and a full snapshot each time is what
            // makes it impractical.

            case 'shardVersions': {
                // What this client already holds, per shard — hand it to the
                // server so it can send back only what moved.
                const ctx = getCtx(args.path);
                const json = await callStr('lucivy_shard_versions', ctx);
                const parsed = json ? JSON.parse(json) : [];
                // The C side reports failure as {"error": "..."}, which is a
                // perfectly valid parse — returning it as data would hand the
                // caller an object where it expects a list.
                if (!Array.isArray(parsed)) throw new Error(parsed.error || 'shard_versions failed');
                result = parsed;
                break;
            }

            case 'exportShardedDelta': {
                const ctx = getCtx(args.path);
                const versions = JSON.stringify(args.clientVersions || []);
                const lenPtr = Module._malloc(4);
                const dataPtr = await Module.ccall('lucivy_export_sharded_delta', 'number',
                    ['number', 'string', 'number'], [ctx, versions, lenPtr], { async: true });
                if (!dataPtr) {
                    Module._free(lenPtr);
                    throw new Error('export_sharded_delta failed — check the client versions shape');
                }
                const len = Module.getValue(lenPtr, 'i32');
                Module._free(lenPtr);
                // slice(), not subarray(): the wasm heap moves when it grows,
                // and a view into it would silently point elsewhere afterwards.
                result = Module.HEAPU8.slice(dataPtr, dataPtr + len);
                break;
            }

            case 'applyShardedDelta': {
                const ctx = getCtx(args.path);
                const data = args.data instanceof Uint8Array ? args.data : new Uint8Array(args.data);
                const ptr = Module._malloc(data.length);
                Module.HEAPU8.set(data, ptr);
                let res;
                try {
                    res = await callStr('lucivy_apply_sharded_delta', ctx, ptr, data.length);
                } finally {
                    Module._free(ptr);
                }
                checkResult(res);
                result = true;
                break;
            }

            case 'numDocs': {
                const ctx = getCtx(args.path);
                result = await Module.ccall('lucivy_num_docs', 'number', ['number'], [ctx], { async: true });
                break;
            }

            case 'schema': {
                const ctx = getCtx(args.path);
                const json = await callStr('lucivy_schema_json', ctx);
                result = json ? JSON.parse(json) : null;
                break;
            }

            default:
                throw new Error(`Unknown message type: ${type}`);
        }

        self.postMessage({ id, result });
    } catch (err) {
        self.postMessage({ id, error: err.message || String(err) });
    }
};
