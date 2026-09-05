# lucivy-wasm 3.0.8

Fast BM25 full-text search for browsers — WASM build with **threading** (emscripten pthreads), OPFS persistence, and snapshot/delta sync support. Runs in a Web Worker.

[**Try the live playground**](https://l-defraiteur.github.io/lucivy/) — it clones lucivy's own source from GitHub and indexes it in your browser.

### What's new in 3.0.0

Measured on 10 000 Linux kernel files, in the browser, 8 threads:

- **Indexing in 55 s** (was ~25 min) and **~1.5x the native query time**
  (124-133 ms per query, median 69-92 ms; native 79 / 49) — the engine now
  runs on **mimalloc**; emscripten's default allocator serialised every thread
  on one lock.
- **SFX v3 index format** with denser sidecars (−22 %), exact highlights on
  every query mode, `parse` boolean syntax, `queryWarnings`, fuzzy by
  Levenshtein or **Jaro-Winkler** (`fuzzy_metric`, `min_similarity`).
- **Memory made explicit**: `memoryStatus()` says whether the index is held
  in memory (up to 3 GB by default) or streamed from OPFS, and `preload()`
  loads it once, after every background merge is done. A page that has just
  indexed several GB cannot also serve them (4 GB address space): persist,
  reload, open.
- Bounded by construction: SFX segment budget, two segment builds at a time,
  merges capped at 800 documents (48 small segments fill eight threads where
  19 large ones fed one), at most 512 documents queued.

### What's new in v2

- **SFX-only engine** — all queries route through the Suffix FST, no legacy code paths
- **Distributed search** — `merge_stats` for multi-node BM25
- **Correct BM25 cross-shard** — identical scores whether 1 shard or 4
- **5 bindings** — Python, Node.js, C++, WASM, Rust

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

const lucivy = new Lucivy('./lucivy-worker.js');
await lucivy.ready;

const index = await lucivy.create('/my-index', {
    fields: [
        { name: 'title', type: 'text' },
        { name: 'body', type: 'text' },
    ],
    shards: 4,
});

await index.add(1, { title: 'Rust Programming', body: 'Systems programming with memory safety' });
await index.add(2, { title: 'Python Guide', body: 'Data science and web development' });
await index.commit();

const results = await index.search(
    { type: 'contains', field: 'body', value: 'program' },
    { highlights: true, fields: true }
);
for (const r of results) {
    console.log(r.docId, r.score, r.fields.title);
}

lucivy.terminate();
```

## API

### Lucivy (main class)

```javascript
import { Lucivy } from 'lucivy-wasm';

const lucivy = new Lucivy('./lucivy-worker.js');
await lucivy.ready;

// Create a new index (config object with fields and optional shards)
const index = await lucivy.create('/my-index', {
    fields: [
        { name: 'title', type: 'text' },
        { name: 'body', type: 'text' },
    ],
    shards: 4,
    // shared_dictionary: true — one dictionary per shard instead of one per
    // segment: about 20 % less OPFS and memory, queries slightly slower at
    // cold cache (roughly x1.2 to x1.6 on exact queries, fuzzy ones faster),
    // same answers. Fixed at creation. Off by default.
    // derived_in_ram: true — the three derived sidecars of each segment
    // (about a third of the index) are rebuilt in memory when the index
    // opens instead of written to OPFS. Same answers. Off by default.
});

// Open an existing index from OPFS
const index2 = await lucivy.open('/my-index');

// Import from a LUCE snapshot (Uint8Array)
const index3 = await lucivy.importSnapshot(snapshotData, '/restored');

// Terminate the worker (frees all WASM memory)
lucivy.terminate();
```

#### Startup options

`new Lucivy(workerUrl, options)` — every option maps to a module argument
read before the engine starts; the defaults are the measured ones:

| option | default | effect |
|---|---|---|
| `noOpfs` | `false` | in-memory filesystem, the index lives for the session |
| `verbose` | `false` | engine diagnostics (`LUCIVY_VERBOSE`, `V3_PROFILE`) |
| `fileCacheMb` | index size | whole-file cache; pins the budget when set |
| `ramIndexMaxMb` | `3072` | above this the index is streamed from OPFS instead of held |
| `schedulerThreads` | `min(cores, 8)` | luciole scheduler pool (12 gains nothing over 8) |
| `writerThreads` | `1` | indexer threads; the writer heap follows |
| `maxMergedDocs` | `800` | largest segment a background merge may produce |
| `maxBuilds` | `2` | segment builds at once (four exhaust the address space) |

#### Memory

```javascript
const st = await index.memoryStatus();
// { index_bytes, in_memory, num_docs, warnings: [..], shards: [..] }
if (st.in_memory) {
    const pre = await index.preload();   // { bytes, files, ms, skipped }
}
```

`preload()` waits for background merges to be quiet, then reads every file
of the index into memory once — a single substring query opens nearly every
sidecar of every segment, so lazy reading only hides that cost in the first
search. It does nothing when the index is streamed. `warnings` carries the
sentence to show a user when the index does not fit.

### LucivyIndex

#### Add / update / delete

```javascript
await index.add(1, { title: 'Hello', body: 'World' });

await index.addMany([
    { docId: 2, title: 'Foo', body: 'Bar' },
    { docId: 3, title: 'Baz', body: 'Qux' },
]);

await index.update(1, { title: 'Updated', body: 'Content' });
await index.remove(2);
await index.commit();
await index.drainMerges();  // wait for background segment merges
```

#### Search

All substring queries are cross-token: they match across token boundaries.

```javascript
// Substring — matches "programming", "programmer", "getProgramHandle", etc.
const results = await index.search(
    { type: 'contains', field: 'body', value: 'program' },
    { highlights: true }
);

// Fuzzy substring (Levenshtein distance)
await index.search({ type: 'contains', field: 'body', value: 'mutx', distance: 1 });

// Regex substring — cross-token regex matching
await index.search({ type: 'contains', field: 'body', value: 'lock.*mutex', regex: true });

// Prefix / startsWith
await index.search({ type: 'startsWith', field: 'body', value: 'prog' });

// Multi-word search — each word as contains, combined with OR
await index.search({ type: 'contains_split', field: 'body', value: 'rust safety' });

// Multi-word with fuzzy distance
await index.search({ type: 'contains_split', field: 'body', value: 'memry safty', distance: 1 });

// Phrase — adjacent tokens in order
await index.search({ type: 'phrase', field: 'body', value: 'mutex lock' });

// Boolean
await index.search({
    type: 'boolean',
    must: [{ type: 'contains', field: 'body', value: 'rust' }],
    must_not: [{ type: 'contains', field: 'body', value: 'deprecated' }],
});

// Retrieve stored fields with results
const results2 = await index.search(
    { type: 'contains', field: 'body', value: 'rust' },
    { fields: true, limit: 10 }
);

// Pre-filtered by doc IDs
const results3 = await index.searchFiltered(
    { type: 'contains', field: 'body', value: 'rust' },
    [1, 3, 5],
    { highlights: true, fields: true }
);
```

**Filtering** on non-text fields:

```javascript
await index.search({
    type: 'contains', field: 'body', value: 'lock',
    filters: [
        { field: 'category', op: 'eq', value: 'kernel' },
        { field: 'score', op: 'gte', value: 0.5 },
    ]
});
```

Filter ops: `eq`, `ne`, `lt`, `lte`, `gt`, `gte`, `in`, `not_in`, `between`, `starts_with`, `contains`.

#### Metadata

```javascript
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
await index.close();    // remove from tracking (OPFS files kept)
await index.destroy();  // remove from tracking + delete OPFS files
lucivy.terminate();     // kill worker, free all WASM memory
```

> **Note**: Always call `lucivy.terminate()` when done. Individual `close()`/`destroy()` are instant and non-blocking. Actual WASM memory is reclaimed on `terminate()`.

## License

MIT
