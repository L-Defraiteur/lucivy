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
    // Drain Rust logs before each call for observability
    await drainRustLogs();
    wlog('[callStr] calling ' + fn);
    const types = args.map(a => typeof a === 'number' ? 'number' : 'string');
    const ptr = await Module.ccall(fn, 'number', types, args, { async: true });
    wlog('[callStr] ' + fn + ' returned');
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
                const { default: createLucivy } = await import('../pkg/lucivy.js');
                Module = await createLucivy();

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
                try {
                    const allJson = await callStr('lucivy_export_all', ctx);
                    const allFiles = JSON.parse(allJson);
                    const modified = allFiles.map(([name, b64]) => [name, base64ToUint8Array(b64)]);
                    await writeFiles(path, modified, []);
                } catch (e) {
                    console.warn('[lucivy-worker] OPFS initial sync skipped:', e.message);
                }

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

                // Synchronous commit via ASYNCIFY (avoids deadlocks with actor system).
                wlog('[commit] starting...');
                const res = await callStr('lucivy_commit', ctx);
                checkResult(res);
                wlog('[commit] done');

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

            case 'drainMerges': {
                const ctx = getCtx(args.path);
                const res = await callStr('lucivy_drain_merges', ctx);
                checkResult(res);
                result = true;
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
