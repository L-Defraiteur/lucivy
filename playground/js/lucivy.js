// lucivy.js — Main thread Promise API for lucivy-emscripten.
//
// Usage:
//   import { Lucivy } from './lucivy.js';
//
//   const lucivy = new Lucivy('./lucivy-worker.js');
//   await lucivy.ready;
//
//   const index = await lucivy.create('/my-index', [
//       { name: 'title', type: 'text' },
//       { name: 'body', type: 'text' },
//   ], 'english');
//
//   await index.add(1, { title: 'Hello', body: 'World' });
//   await index.commit();
//   const results = await index.search('hello');

export class Lucivy {
    constructor(workerUrl) {
        this._worker = new Worker(workerUrl, { type: 'module' });
        this._nextId = 1;
        this._pending = new Map();

        this._logRingInterval = null;

        this._worker.onmessage = (e) => {
            // Relay worker debug logs to main thread console
            if (e.data.type === 'log') {
                console.log('[worker]', e.data.msg);
                return;
            }
            // Set up SharedArrayBuffer log ring polling (reads Rust logs
            // directly from WASM memory — works even during deadlocks).
            if (e.data.type === 'logRing') {
                this._startLogRingPoller(e.data.buffer, e.data.ringPtr, e.data.ringSize);
                return;
            }
            const { id, result, error } = e.data;
            const pending = this._pending.get(id);
            if (pending) {
                this._pending.delete(id);
                if (error) pending.reject(new Error(error));
                else pending.resolve(result);
            }
        };

        this.ready = this._call('init');
    }

    _startLogRingPoller(sab, ringPtr, ringSize) {
        // Atomics views for the two u32 header fields
        const writePosView = new Int32Array(sab, ringPtr, 1);
        const wrapCountView = new Int32Array(sab, ringPtr + 4, 1);
        const bytes = new Uint8Array(sab, ringPtr, ringSize);
        let readPos = 8;
        let lastWrap = 0;

        this._logRingInterval = setInterval(() => {
            try {
                const wrap = Atomics.load(wrapCountView, 0);
                if (wrap !== lastWrap) {
                    readPos = 8;
                    lastWrap = wrap;
                }
                const writePos = Atomics.load(writePosView, 0);
                while (readPos + 2 <= writePos && readPos + 2 < ringSize) {
                    const len = bytes[readPos] | (bytes[readPos + 1] << 8);
                    if (len === 0 || readPos + 2 + len > ringSize) break;
                    const msgBytes = bytes.slice(readPos + 2, readPos + 2 + len);
                    const msg = new TextDecoder().decode(msgBytes);
                    console.log('[rust]', msg);
                    readPos += 2 + len;
                }
            } catch (e) { /* ignore read errors */ }
        }, 50);
    }

    _call(type, args = {}) {
        return new Promise((resolve, reject) => {
            const id = this._nextId++;
            this._pending.set(id, { resolve, reject });
            this._worker.postMessage({ type, id, ...args });
        });
    }

    /**
     * Create a new index.
     * @param {string} path
     * @param {Array|Object} fieldsOrConfig — either a fields array or a full SchemaConfig
     *   Full config: { fields: [...], sfx: false, tokenizer: "english", ... }
     *   Legacy: [{ name: "body", type: "text" }]
     */
    async create(path, fieldsOrConfig) {
        const isConfig = !Array.isArray(fieldsOrConfig) && typeof fieldsOrConfig === 'object'
            && fieldsOrConfig.fields;
        if (isConfig) {
            await this._call('create', { path, config: fieldsOrConfig });
        } else {
            await this._call('create', { path, fields: fieldsOrConfig });
        }
        return new LucivyIndex(this, path);
    }

    async open(path) {
        await this._call('open', { path });
        return new LucivyIndex(this, path);
    }

    async importSnapshot(data, path) {
        const res = await this._call('importSnapshot', { data, path });
        return new LucivyIndex(this, path);
    }

    terminate() {
        this._worker.terminate();
    }
}

export class LucivyIndex {
    constructor(lucivy, path) {
        this._lucivy = lucivy;
        this.path = path;
    }

    add(docId, fields) {
        return this._lucivy._call('add', { path: this.path, docId, fields });
    }

    addMany(docs) {
        return this._lucivy._call('addMany', { path: this.path, docs });
    }

    remove(docId) {
        return this._lucivy._call('remove', { path: this.path, docId });
    }

    update(docId, fields) {
        return this._lucivy._call('update', { path: this.path, docId, fields });
    }

    commit() {
        return this._lucivy._call('commit', { path: this.path });
    }

    rollback() {
        return this._lucivy._call('rollback', { path: this.path });
    }

    search(query, options = {}) {
        return this._lucivy._call('search', {
            path: this.path,
            query,
            limit: options.limit,
            highlights: options.highlights,
            fields: options.fields,
        });
    }

    searchFiltered(query, allowedIds, options = {}) {
        return this._lucivy._call('searchFiltered', {
            path: this.path,
            query,
            allowedIds,
            limit: options.limit,
            highlights: options.highlights,
            fields: options.fields,
        });
    }

    numDocs() {
        return this._lucivy._call('numDocs', { path: this.path });
    }

    schema() {
        return this._lucivy._call('schema', { path: this.path });
    }

    exportSnapshot() {
        return this._lucivy._call('exportSnapshot', { path: this.path });
    }

    close() {
        return this._lucivy._call('close', { path: this.path });
    }

    destroy() {
        return this._lucivy._call('destroy', { path: this.path });
    }
}
