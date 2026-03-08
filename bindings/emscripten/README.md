# lucivy-wasm

Fast BM25 full-text search for browsers — WASM build of Lucivy with **threading** (emscripten pthreads), OPFS persistence, and LUCE snapshot support. Runs in a Web Worker.

> **v0.4.0**: Search results can now include stored field values via `{ fields: true }` option.
>
> **v0.3.0 breaking change**: This package now uses the emscripten build with threading support. The API is now worker-based (async). See below for migration.

## Install

```bash
npm install lucivy-wasm
```

## Requirements

- **COOP/COEP headers** required for `SharedArrayBuffer` (threading):
  - `Cross-Origin-Opener-Policy: same-origin`
  - `Cross-Origin-Embedder-Policy: require-corp`
- For GitHub Pages or environments without header control, use [coi-serviceworker](https://github.com/nicojuicy/coi-serviceworker).

## Quick start

```javascript
import { Lucivy } from 'lucivy-wasm';

const lucivy = new Lucivy(new URL('lucivy-wasm/worker', import.meta.url));
await lucivy.ready;

const index = await lucivy.create('/my-index', [
    { name: 'title', type: 'text' },
    { name: 'body', type: 'text' },
], 'english');

await index.add(1, { title: 'Rust Programming', body: 'Systems programming with memory safety' });
await index.add(2, { title: 'Python Guide', body: 'Data science and web development' });
await index.commit();

const results = await index.search('programming');
for (const r of results) {
    console.log(r.docId, r.score);
}

// Cleanup
lucivy.terminate();
```

## API

### Lucivy (main class)

```javascript
import { Lucivy } from 'lucivy-wasm';

const lucivy = new Lucivy('./path/to/lucivy-worker.js');
await lucivy.ready;

// Create a new index
const index = await lucivy.create('/my-index', [
    { name: 'title', type: 'text' },
    { name: 'body', type: 'text' },
], 'english');

// Open an existing index from OPFS
const index2 = await lucivy.open('/my-index');

// Import from a LUCE snapshot
const index3 = await lucivy.importSnapshot(snapshotBlob, '/restored');

// Terminate the worker (frees all WASM memory)
lucivy.terminate();
```

### LucivyIndex

```javascript
// Add / update / delete
await index.add(1, { title: 'Hello', body: 'World' });
await index.addMany([
    { docId: 2, title: 'Foo', body: 'Bar' },
    { docId: 3, title: 'Baz', body: 'Qux' },
]);
await index.update(1, { title: 'Updated', body: 'Content' });
await index.remove(2);
await index.commit();

// Search (BM25)
const results = await index.search('rust programming');

// Structured query with highlights
const results2 = await index.search(
    { type: 'contains', field: 'body', value: 'program' },
    { highlights: true }
);

// Return stored fields with results
const results3 = await index.search('programming', { fields: true });
for (const r of results3) {
    console.log(r.fields.title, r.score);  // stored field values
}

// Pre-filtered by doc IDs
const results4 = await index.searchFiltered('programming', [1, 3], { highlights: true, fields: true });

// Metadata
const count = await index.numDocs();
const schema = await index.schema();
```

### Snapshots

```javascript
// Export to Uint8Array (.luce format)
const snapshot = await index.exportSnapshot();

// Import from Uint8Array
const restored = await lucivy.importSnapshot(snapshot, '/restored');
```

### Cleanup

```javascript
await index.close();    // Remove from tracking (OPFS files kept)
await index.destroy();  // Remove from tracking + delete OPFS files
lucivy.terminate();     // Kill worker, free all WASM memory
```

> **Note**: Always call `lucivy.terminate()` when done. Individual `close()`/`destroy()` are instant and non-blocking. Actual WASM memory is reclaimed on `terminate()`.

## Supported stemmers

`english`, `french`, `german`, `spanish`, `italian`, `portuguese`, `dutch`, `russian`

## License

MIT
