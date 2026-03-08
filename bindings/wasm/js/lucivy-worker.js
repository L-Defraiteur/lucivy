// lucivy-worker.js — Web Worker that runs lucivy-wasm with OPFS persistence.
//
// Usage from main thread:
//   const worker = new Worker('lucivy-worker.js', { type: 'module' });
//   worker.postMessage({ type: 'create', id: 1, path: '/my-index', fields: [...], stemmer: 'english' });
//
// Or use lucivy.js for a Promise-based API.

import init, { Index } from '../pkg/lucivy_wasm.js';

let wasmReady = false;
const indexes = new Map(); // path → Index

// ── OPFS helpers ───────────────────────────────────────────────────────────

async function getOpfsDir(path) {
    const root = await navigator.storage.getDirectory();
    // Create nested dirs from path (e.g. "/my-index" → "my-index")
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

// ── Message handler ────────────────────────────────────────────────────────

self.onmessage = async (e) => {
    const { type, id, ...args } = e.data;

    try {
        if (!wasmReady && type !== 'init') {
            throw new Error('WASM not initialized — send {type: "init"} first');
        }

        let result;

        switch (type) {
            case 'init': {
                await init(args.wasmUrl);
                wasmReady = true;
                result = true;
                break;
            }

            case 'create': {
                const { path, fields, stemmer } = args;
                const fieldsJson = typeof fields === 'string' ? fields : JSON.stringify(fields);
                const idx = new Index(path, fieldsJson, stemmer || '');
                indexes.set(path, idx);

                // Persist initial files to OPFS.
                const allFiles = idx.exportAllFiles();
                const modified = allFiles.map(([name, data]) => [name, data]);
                await writeFiles(path, modified, []);

                result = { path, numDocs: idx.numDocs };
                break;
            }

            case 'open': {
                const { path } = args;
                const files = await readAllFiles(path);
                if (files.size === 0) {
                    throw new Error(`No index found at OPFS path: ${path}`);
                }
                const idx = Index.open(path, files);
                indexes.set(path, idx);
                result = { path, numDocs: idx.numDocs };
                break;
            }

            case 'add': {
                const idx = getIndex(args.path);
                const fieldsJson = typeof args.fields === 'string'
                    ? args.fields : JSON.stringify(args.fields);
                idx.add(args.docId, fieldsJson);
                result = true;
                break;
            }

            case 'addMany': {
                const idx = getIndex(args.path);
                const docsJson = typeof args.docs === 'string'
                    ? args.docs : JSON.stringify(args.docs);
                idx.addMany(docsJson);
                result = true;
                break;
            }

            case 'remove': {
                const idx = getIndex(args.path);
                idx.remove(args.docId);
                result = true;
                break;
            }

            case 'update': {
                const idx = getIndex(args.path);
                const fieldsJson = typeof args.fields === 'string'
                    ? args.fields : JSON.stringify(args.fields);
                idx.update(args.docId, fieldsJson);
                result = true;
                break;
            }

            case 'commit': {
                const idx = getIndex(args.path);
                idx.commit();

                // Sync dirty files to OPFS.
                const dirty = idx.exportDirtyFiles();
                await writeFiles(args.path, dirty.modified, dirty.deleted);

                result = { numDocs: idx.numDocs };
                break;
            }

            case 'rollback': {
                const idx = getIndex(args.path);
                idx.rollback();
                result = true;
                break;
            }

            case 'search': {
                const idx = getIndex(args.path);
                const queryJson = typeof args.query === 'string' && !args.query.startsWith('{')
                    ? JSON.stringify(args.query)
                    : (typeof args.query === 'object' ? JSON.stringify(args.query) : args.query);
                const json = idx.search(queryJson, args.limit || 10, args.highlights || false);
                result = JSON.parse(json);
                break;
            }

            case 'searchFiltered': {
                const idx = getIndex(args.path);
                const queryJson = typeof args.query === 'string' && !args.query.startsWith('{')
                    ? JSON.stringify(args.query)
                    : (typeof args.query === 'object' ? JSON.stringify(args.query) : args.query);
                const ids = new Uint32Array(args.allowedIds);
                const json = idx.searchFiltered(
                    queryJson, args.limit || 10, ids, args.highlights || false);
                result = JSON.parse(json);
                break;
            }

            case 'close': {
                indexes.delete(args.path);
                result = true;
                break;
            }

            case 'destroy': {
                indexes.delete(args.path);
                await removeAllFiles(args.path);
                result = true;
                break;
            }

            case 'numDocs': {
                const idx = getIndex(args.path);
                result = idx.numDocs;
                break;
            }

            case 'schema': {
                const idx = getIndex(args.path);
                result = JSON.parse(idx.schemaJson);
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

function getIndex(path) {
    const idx = indexes.get(path);
    if (!idx) throw new Error(`No index open at path: ${path}`);
    return idx;
}
