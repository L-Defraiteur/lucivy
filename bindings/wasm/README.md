# @lucivy/wasm

Fast BM25 full-text search for browsers — WASM build of Lucivy with OPFS persistence. Runs in a Web Worker.

## Install

```bash
npm install @lucivy/wasm
```

## Quick start

```javascript
import init, { Index } from '@lucivy/wasm';

await init();

const index = new Index('/my_index', JSON.stringify([
    { name: 'title', type: 'text' },
    { name: 'body', type: 'text' },
]));

index.add(1, JSON.stringify({ title: 'Rust Programming', body: 'Systems programming with memory safety' }));
index.add(2, JSON.stringify({ title: 'Python Guide', body: 'Data science and web development' }));
index.commit();

const results = JSON.parse(index.search(JSON.stringify('programming'), 10, true));
for (const r of results) {
    console.log(r.doc_id, r.score, r.highlights);
}
```

## API

### Create / open

```javascript
// Create a new index
const index = new Index('/my_index', JSON.stringify([
    { name: 'title', type: 'text' },
    { name: 'body',  type: 'text' },
    { name: 'tag',   type: 'keyword' },
]));

// Open from OPFS files (Map of filename → Uint8Array)
const index2 = Index.open('/my_index', filesMap);
```

### Add / update / delete

```javascript
index.add(1, JSON.stringify({ title: 'Hello', body: 'World' }));
index.addMany(JSON.stringify([
    { docId: 2, title: 'Foo', body: 'Bar' },
    { docId: 3, title: 'Baz', body: 'Qux' },
]));
index.update(1, JSON.stringify({ title: 'Updated', body: 'Content' }));
index.remove(2);
index.commit();
```

### Search

```javascript
// String query (contains_split across all text fields)
const results = JSON.parse(index.search(JSON.stringify('rust programming'), 10));

// Structured query
const results2 = JSON.parse(index.search(JSON.stringify({
    type: 'contains', field: 'body', value: 'program'
}), 10, true));

// Pre-filtered by doc IDs
const results3 = JSON.parse(index.searchFiltered(
    JSON.stringify('programming'), 10, new Uint32Array([1, 3]), true
));
```

### OPFS persistence

```javascript
// Get dirty files (modified since last export) for incremental OPFS sync
const dirty = index.exportDirtyFiles(); // Map<string, Uint8Array>

// Get all files
const all = index.exportAllFiles(); // Map<string, Uint8Array>
```

### Snapshots (export / import)

```javascript
// Export to Uint8Array (.luce format)
const snapshot = index.exportSnapshot();

// Import from Uint8Array
const restored = Index.importSnapshot(snapshot, '/restored_index');
```

### Properties

```javascript
index.numDocs     // number of documents
index.path        // index path
index.schemaJson  // JSON string of field definitions
```

## OPFS + Web Worker pattern

This package is designed to run in a Web Worker with OPFS for persistence:

1. On startup: read files from OPFS → `Index.open(path, filesMap)`
2. After mutations + commit: `index.exportDirtyFiles()` → write to OPFS
3. For backup/transfer: `index.exportSnapshot()` → portable `.luce` blob

## License

MIT
