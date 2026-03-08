// lucivy-worker.js — Web Worker that runs lucivy-emscripten with OPFS persistence.
//
// Usage from main thread:
//   const worker = new Worker('lucivy-worker.js', { type: 'module' });
//   worker.postMessage({ type: 'init', id: 1 });
//
// Or use lucivy.js for a Promise-based API.

// The emscripten module must be in ../pkg/lucivy.js relative to this worker.
// We use importScripts-compatible dynamic import.
let Module = null;

const indexes = new Map(); // path -> ctx pointer

// ── Helpers ──────────────────────────────────────────────────────────────────

function callStr(fn, ...args) {
    const types = args.map(a => typeof a === 'number' ? 'number' : 'string');
    const ptr = Module.ccall(fn, 'number', types, args);
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
    const dirtyJson = callStr('lucivy_export_dirty', ctx);
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
                // Dynamic import of the emscripten module.
                // The worker URL determines the base for relative imports.
                const { default: createLucivy } = await import('../pkg/lucivy.js');
                Module = await createLucivy();
                result = true;
                break;
            }

            case 'create': {
                const { path, fields, stemmer } = args;
                const fieldsJson = typeof fields === 'string' ? fields : JSON.stringify(fields);
                const ctx = Module.ccall('lucivy_create', 'number',
                    ['string', 'string', 'string'],
                    [path, fieldsJson, stemmer || '']);
                if (!ctx) throw new Error('lucivy_create returned null');
                indexes.set(path, ctx);

                // Export all files to OPFS for initial persistence (best-effort).
                try {
                    const allJson = callStr('lucivy_export_all', ctx);
                    const allFiles = JSON.parse(allJson);
                    const modified = allFiles.map(([name, b64]) => [name, base64ToUint8Array(b64)]);
                    await writeFiles(path, modified, []);
                } catch (e) {
                    console.warn('[lucivy-worker] OPFS initial sync skipped:', e.message);
                }

                result = { path, numDocs: Module.ccall('lucivy_num_docs', 'number', ['number'], [ctx]) };
                break;
            }

            case 'open': {
                const { path } = args;
                const files = await readAllFiles(path);
                if (files.size === 0) {
                    throw new Error(`No index found at OPFS path: ${path}`);
                }

                // Two-phase open: begin → import files → finish.
                const openCtx = Module.ccall('lucivy_open_begin', 'number', ['string'], [path]);
                if (!openCtx) throw new Error('lucivy_open_begin returned null');

                for (const [name, data] of files) {
                    // Allocate memory and copy file data.
                    const ptr = Module._malloc(data.length);
                    Module.HEAPU8.set(data, ptr);
                    // Write file name as UTF8.
                    const nameBytes = Module.lengthBytesUTF8(name) + 1;
                    const namePtr = Module._malloc(nameBytes);
                    Module.stringToUTF8(name, namePtr, nameBytes);
                    Module.ccall('lucivy_import_file', null,
                        ['number', 'number', 'number', 'number'],
                        [openCtx, namePtr, ptr, data.length]);
                    Module._free(namePtr);
                    Module._free(ptr);
                }

                const ctx = Module.ccall('lucivy_open_finish', 'number', ['number'], [openCtx]);
                if (!ctx) throw new Error('lucivy_open_finish returned null');
                indexes.set(path, ctx);

                result = { path, numDocs: Module.ccall('lucivy_num_docs', 'number', ['number'], [ctx]) };
                break;
            }

            case 'add': {
                const ctx = getCtx(args.path);
                const fieldsJson = typeof args.fields === 'string'
                    ? args.fields : JSON.stringify(args.fields);
                const res = callStr('lucivy_add', ctx, args.docId, fieldsJson);
                checkResult(res);
                result = true;
                break;
            }

            case 'addMany': {
                const ctx = getCtx(args.path);
                const docsJson = typeof args.docs === 'string'
                    ? args.docs : JSON.stringify(args.docs);
                const res = callStr('lucivy_add_many', ctx, docsJson);
                checkResult(res);
                result = true;
                break;
            }

            case 'remove': {
                const ctx = getCtx(args.path);
                const res = callStr('lucivy_remove', ctx, args.docId);
                checkResult(res);
                result = true;
                break;
            }

            case 'update': {
                const ctx = getCtx(args.path);
                const fieldsJson = typeof args.fields === 'string'
                    ? args.fields : JSON.stringify(args.fields);
                const res = callStr('lucivy_update', ctx, args.docId, fieldsJson);
                checkResult(res);
                result = true;
                break;
            }

            case 'commit': {
                const ctx = getCtx(args.path);
                const res = callStr('lucivy_commit', ctx);
                checkResult(res);

                // Sync dirty files to OPFS (best-effort — may not be available in all contexts).
                try {
                    await syncDirtyToOpfs(args.path, ctx);
                } catch (e) {
                    console.warn('[lucivy-worker] OPFS sync skipped:', e.message);
                }

                result = { numDocs: Module.ccall('lucivy_num_docs', 'number', ['number'], [ctx]) };
                break;
            }

            case 'rollback': {
                const ctx = getCtx(args.path);
                const res = callStr('lucivy_rollback', ctx);
                checkResult(res);
                result = true;
                break;
            }

            case 'search': {
                const ctx = getCtx(args.path);
                const queryJson = typeof args.query === 'string' && !args.query.startsWith('{')
                    ? JSON.stringify(args.query)
                    : (typeof args.query === 'object' ? JSON.stringify(args.query) : args.query);
                const json = callStr('lucivy_search', ctx, queryJson, args.limit || 10, args.highlights ? 1 : 0);
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

                const resPtr = Module.ccall('lucivy_search_filtered', 'number',
                    ['number', 'string', 'number', 'number', 'number', 'number'],
                    [ctx, queryJson, args.limit || 10, idsPtr, ids.length, args.highlights ? 1 : 0]);
                const json = Module.UTF8ToString(resPtr);
                Module._free(idsPtr);

                result = JSON.parse(json);
                if (result.error) throw new Error(result.error);
                break;
            }

            case 'close': {
                // Remove from tracking. The Rust context is intentionally NOT destroyed
                // here because lucivy_destroy can deadlock under emscripten pthreads
                // (IndexWriter drop joins threads via the event loop).
                // All WASM memory is fully reclaimed when the worker is terminated.
                indexes.delete(args.path);
                result = true;
                break;
            }

            case 'destroy': {
                indexes.delete(args.path);
                // OPFS cleanup is best-effort (may hang if storage API unavailable)
                removeAllFiles(args.path).catch(() => {});
                result = true;
                break;
            }

            case 'exportSnapshot': {
                const ctx = getCtx(args.path);
                const lenPtr = Module._malloc(4);
                const dataPtr = Module.ccall('lucivy_export_snapshot', 'number',
                    ['number', 'number'], [ctx, lenPtr]);
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
                const ctx = Module.ccall('lucivy_import_snapshot', 'number',
                    ['number', 'number', 'string'], [ptr, dataArr.length, path]);
                Module._free(ptr);
                if (!ctx) throw new Error('import_snapshot failed — invalid snapshot data');
                indexes.set(path, ctx);
                result = { path, numDocs: Module.ccall('lucivy_num_docs', 'number', ['number'], [ctx]) };
                break;
            }

            case 'numDocs': {
                const ctx = getCtx(args.path);
                result = Module.ccall('lucivy_num_docs', 'number', ['number'], [ctx]);
                break;
            }

            case 'schema': {
                const ctx = getCtx(args.path);
                const json = callStr('lucivy_schema_json', ctx);
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
