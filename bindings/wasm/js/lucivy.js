// lucivy.js — Main thread API for lucivy-wasm.
//
// Usage:
//   import { Lucivy } from './lucivy.js';
//
//   const lucivy = new Lucivy('lucivy-worker.js', 'lucivy_wasm_bg.wasm');
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
    constructor(workerUrl, wasmUrl) {
        this._worker = new Worker(workerUrl, { type: 'module' });
        this._nextId = 1;
        this._pending = new Map();

        this._worker.onmessage = (e) => {
            const { id, result, error } = e.data;
            const pending = this._pending.get(id);
            if (pending) {
                this._pending.delete(id);
                if (error) pending.reject(new Error(error));
                else pending.resolve(result);
            }
        };

        this.ready = this._call('init', { wasmUrl });
    }

    _call(type, args = {}) {
        return new Promise((resolve, reject) => {
            const id = this._nextId++;
            this._pending.set(id, { resolve, reject });
            this._worker.postMessage({ type, id, ...args });
        });
    }

    async create(path, fields, stemmer) {
        await this._call('create', { path, fields, stemmer });
        return new LucivyIndex(this, path);
    }

    async open(path) {
        await this._call('open', { path });
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
        });
    }

    searchFiltered(query, allowedIds, options = {}) {
        return this._lucivy._call('searchFiltered', {
            path: this.path,
            query,
            allowedIds,
            limit: options.limit,
            highlights: options.highlights,
        });
    }

    numDocs() {
        return this._lucivy._call('numDocs', { path: this.path });
    }

    schema() {
        return this._lucivy._call('schema', { path: this.path });
    }

    close() {
        return this._lucivy._call('close', { path: this.path });
    }

    destroy() {
        return this._lucivy._call('destroy', { path: this.path });
    }
}
