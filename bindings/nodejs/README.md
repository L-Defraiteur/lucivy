# lucivy 3.0.4

Fast BM25 full-text search for Node.js — with substring matching, fuzzy search, regex, and highlights. Powered by Rust via napi-rs.

[**Try the live playground**](https://l-defraiteur.github.io/lucivy/) — runs entirely in your browser via WASM.

### What's new in 3.0.0

- **SFX v3 segment format** — per-field suffix FST files, the default for every new index; v2 indexes still open
- **`parse` query type** — boolean syntax (`AND` / `OR` / `NOT`, quotes, `+` / `-`, parentheses) on top of substring matching
- **`queryWarnings()`** — what the engine will really search, and where it falls back to a scan, before running the query
- **Snapshots served in place** — `Index.openSnapshot()` reads a LUCE blob without extracting it
- **Maintenance** — `compact()`, `waitMergesQuiet()`, `indexBytes()`, `dropIndex()`
- **Bring your own storage** — `BlobIndex` keeps an index in the transactional store of your choice through a plain object of callbacks; asynchronous API

Still there from 2.x: SFX-only engine, distributed search (`exportStats` / `mergeStats` / `searchWithGlobalStats`), incremental LUCIDS delta sync, BM25 scores identical across 1 or N shards, bindings for Python, Node.js, C++, WASM and Rust.

## Install

```bash
npm install lucivy@3.0.0
```

## Quick start

```javascript
const { Index } = require('lucivy');

const index = Index.create('/tmp/my_index', [
    { name: 'title', type: 'text', stored: true },
    { name: 'body', type: 'text', stored: true },
]);

index.add(1, { title: 'Rust Programming', body: 'Systems programming with memory safety' });
index.add(2, { title: 'Python Guide', body: 'Data science and web development' });
index.commit();

const results = index.search('programming', { highlights: true });
for (const r of results) {
    console.log(r.docId, r.score, r.highlights);
}
```

## API

### Create / open

```javascript
const index = Index.create('/tmp/my_index', [
    { name: 'title', type: 'text', stored: true },
    { name: 'body',  type: 'text', stored: true },
    { name: 'score', type: 'f64', fast: true },
]);

// Sharded (4 shards)
const sharded = Index.create('/tmp/sharded', [...], 4);

// Open existing
const index2 = Index.open('/tmp/my_index');
```

Field types: `"text"` (full-text, tokenized), `"u64"`, `"i64"`, `"f64"`, `"bool"`, `"date"`.

### Add / update / delete

```javascript
index.add(1, { title: 'Hello', body: 'World', score: 3.14 });

index.addMany([
    { docId: 2, title: 'Foo', body: 'Bar' },
    { docId: 3, title: 'Baz', body: 'Qux' },
]);

index.update(1, { title: 'Updated', body: 'Content' });
index.delete(2);
index.commit();
```

### Search

```javascript
// String query — each word searched across all text fields (contains_split)
let results = index.search('rust async programming');

// Options: limit, highlights, allowedIds, fields
results = index.search('rust', { limit: 20, highlights: true, allowedIds: [1, 3] });

// Retrieve stored field values with results
results = index.search('rust', { fields: true });
for (const r of results) {
    console.log(r.docId, r.fields.title, r.fields.body);
}
```

#### contains — substring, fuzzy, regex (cross-token)

All substring queries are cross-token: they match across token boundaries.

```javascript
// Substring — matches "programming", "programmer", "getProgramHandle", etc.
index.search({ type: 'contains', field: 'body', value: 'program' });

// Fuzzy substring (Levenshtein distance)
index.search({ type: 'contains', field: 'body', value: 'mutx', distance: 1 });

// Fuzzy with Jaro-Winkler instead of Levenshtein: candidates come from the
// trigram pigeonhole at `distance` (default 2), Jaro-Winkler decides, and
// hits are tiered by similarity (a typo at the end ranks above one at the start)
index.search({ type: 'fuzzy', field: 'body', value: 'kmalloc', fuzzy_metric: 'jaro_winkler', min_similarity: 0.9 });

// Regex substring — cross-token regex matching
index.search({ type: 'contains', field: 'body', value: 'lock.*mutex', regex: true });

// Prefix / startsWith — match must start at token boundary (SI=0)
index.search({ type: 'startsWith', field: 'body', value: 'prog' });

// Exact whole-token match
index.search({ type: 'term', field: 'body', value: 'lock' });

// Phrase — adjacent tokens in order
index.search({ type: 'phrase', field: 'body', value: 'mutex lock' });
```

#### contains_split — multi-word search

Split on whitespace, each word becomes a `contains` query, combined with boolean OR.

```javascript
index.search({ type: 'contains_split', field: 'body', value: 'rust safety' });

// With fuzzy distance — each word gets fuzzy tolerance
index.search({ type: 'contains_split', field: 'body', value: 'memry safty', distance: 1 });
```

#### boolean — combine queries with must / should / must_not

```javascript
index.search({
    type: 'boolean',
    must: [
        { type: 'contains', field: 'body', value: 'rust' },
    ],
    should: [
        { type: 'contains', field: 'title', value: 'guide' },
    ],
    must_not: [
        { type: 'contains', field: 'body', value: 'deprecated' },
    ],
});
```

#### parse — boolean syntax in one string

A plain value (no operators) is an OR of substring `contains` queries, one per word
and per field. With boolean syntax the string is compiled into a `boolean` query of
`contains` clauses: `AND` / `OR` / `NOT`, `"quoted phrases"`, `+required` / `-excluded`,
and parentheses. Precedence is `NOT` > `AND` > `OR`; words side by side are OR'd.
Highlights work in both cases. `fields` takes several fields at once.

```javascript
// Plain value: OR of contains per word x field
index.search({ type: 'parse', field: 'body', value: 'kmalloc spin_lock' });

// Boolean syntax
index.search({ type: 'parse', field: 'body', value: 'kmalloc AND NOT vfree' });
index.search({ type: 'parse', field: 'body', value: '"spin_lock" -vfree' });
index.search({ type: 'parse', fields: ['title', 'body'], value: '(mutex OR spinlock) AND init' });
```

#### queryWarnings — know what will run before running it

Returns plain-text warnings for a query, without executing it: separators ignored
in relaxed mode, a fuzzy distance too loose for the query length, a regex without a
usable literal (full scan), segments written by the legacy indexer. Empty array when
nothing applies. For `parse`, the warnings say which of the two modes the value
selected.

```javascript
index.queryWarnings({ type: 'contains', field: 'body', value: 'kmalloc' });
// []
index.queryWarnings({ type: 'contains', field: 'body', value: '__init' });
// ['separators are ignored (strict_separators=false): "__init" is searched as "init"']
index.queryWarnings({ type: 'regex', field: 'body', value: '[0-9]{8}' });
// ['"[0-9]{8}" requires no literal the index can look up: every document is scanned whole (full scan, ...)']
index.queryWarnings({ type: 'fuzzy', field: 'body', value: 'init' });
// ['distance 1 on "init" (4 chars) rewrites a quarter of the query or more: unrelated short words will match (...)']
index.queryWarnings({ type: 'parse', field: 'body', value: 'kmalloc spin' });
// ['parse without boolean operators: "kmalloc spin" runs as OR of substring contains, one per word']
```

#### Filtering

Filter on non-text fields (combined with AND):

```javascript
index.search({
    type: 'contains', field: 'body', value: 'lock',
    filters: [
        { field: 'category', op: 'eq', value: 'kernel' },
        { field: 'score', op: 'gte', value: 0.5 },
        { field: 'status', op: 'in', value: ['active', 'review'] },
    ]
});
```

Filter ops: `eq`, `ne`, `lt`, `lte`, `gt`, `gte`, `in`, `not_in`, `between`, `starts_with`, `contains`.

Pre-filter by document ID (fast, bitmap-based):

```javascript
index.search({ type: 'contains', field: 'body', value: 'lock' }, { allowedIds: [1, 2, 3] });
```

> **Note:** napi-rs converts snake_case to camelCase — use `allowedIds`, `docId`, `numDocs`, etc. in JavaScript.

### Snapshots (export / import)

```javascript
// Export to file
index.exportSnapshotTo('./backup.luce');

// Export to Buffer
const buf = index.exportSnapshot();

// Import from file
const restored = Index.importSnapshotFrom('./backup.luce', './restored_index');

// Import from Buffer
const restored2 = Index.importSnapshot(buf, './restored_index');

// Serve a snapshot in place — nothing extracted, nothing written to disk.
// Memory cost = the blob's length. Read-only: add / delete / commit / compact
// and the snapshot or delta exports throw. Use importSnapshot() for a writable copy.
const served = Index.openSnapshot(buf);
const served2 = Index.openSnapshotFrom('./backup.luce');
served.search('programming');   // same results as the source index
served.path;                    // '' — a served snapshot has no directory
```

### Maintenance

```javascript
// Once after a bulk load: merge every shard's segments into segments of at
// most maxDocs documents (default 10000), then commit. Returns the number of
// merge rounds that reduced a shard's segment count.
const merges = index.compact();          // or index.compact(50000)

// Block until no background merge is running or about to start.
// Returns the number of rounds that still saw activity (0 = already quiet).
index.waitMergesQuiet();

// On-disk bytes of every searchable segment, all shards. Call waitMergesQuiet()
// first for a stable figure.
const bytes = index.indexBytes();

// Delete the whole index: close, then remove its files. The instance is
// consumed — every later call on it throws.
index.dropIndex();
```

### Delta sync (incremental)

Sync only the segments that changed since the client's last version.

```javascript
// Get current shard versions
const versions = index.shardVersions;

// Export delta (only changed segments)
const delta = index.exportShardedDelta(clientVersions);

// Apply delta on the client side
clientIndex.applyShardedDelta(delta);
```

### Distributed search

Run BM25 search across multiple machines with correct IDF.

```javascript
const { mergeStats } = require('lucivy');

const queryJson = '{"type":"contains","field":"body","value":"mutex"}';

// 1. Each node exports its local BM25 stats
const statsA = nodeA.exportStats(queryJson);  // JSON string
const statsB = nodeB.exportStats(queryJson);  // JSON string

// 2. Coordinator merges stats from all nodes
const merged = mergeStats([statsA, statsB]);

// 3. Each node searches with global stats (correct IDF across all nodes)
const resultsA = nodeA.searchWithGlobalStats(queryJson, merged, 10);
const resultsB = nodeB.searchWithGlobalStats(queryJson, merged, 10);

// 4. Coordinator merges top-K results by score
const all = [...resultsA, ...resultsB].sort((a, b) => b.score - a.score).slice(0, 10);
```

### Properties

```javascript
index.numDocs      // number of documents (getter)
index.numShards    // number of shards (getter)
index.path         // index directory path (getter)
index.schema       // array of {name, type} objects (getter)
index.shardVersions // per-shard version info for delta sync (getter)
index.indexBytes() // on-disk size of all searchable segments
index.close()      // flush + release writer lock
index.dropIndex()  // close + delete the index files (instance unusable afterwards)
```

## Bring your own storage (ACID)

`Index` stores its files on the filesystem. `BlobIndex` stores them wherever
you say: every file of the index becomes a blob that your JavaScript object
loads and saves — a transactional database, an object store, a `Map`. The
store is the source of truth; a local directory only caches the blobs for
mmap reads and can be thrown away. `meta.json` is written last at each commit,
so a store with transactions gives you an index that is either at the previous
commit or at the new one, never in between.

### The store object

```javascript
const store = {
    load(indexName, fileName)          // → Buffer | Uint8Array | null (null = does not exist)
    save(indexName, fileName, data)    // data: Buffer; create or overwrite
    delete(indexName, fileName)        // a missing blob is not an error
    exists(indexName, fileName)        // → boolean
    list(indexName)                    // → string[]: every fileName under indexName
    // optional, for lazy loading:
    blobLen(indexName, fileName)       // → number | null (null = unknown)
    loadRange(indexName, fileName, offset, length)  // → Buffer | Uint8Array | null (null = unsupported)
};
```

Each method may return its value directly or a Promise of it. A thrown error
or a rejection is reported as the rejection of the `BlobIndex` call that
needed it. Methods are called with the store as `this`.

`indexName` is a namespace, not just the name you gave: segment files live
under `"Lucivy_<name>/shard_<i>"`, the root files (`_shard_config.json`,
`_shard_stats.bin`) under the bare `"<name>"`. Keep blobs keyed by the pair
`(indexName, fileName)` and `list()` cheap. Opening an index takes the writer
lock through the store (`.lucivy-writer.lock` is saved, then deleted) and
`close()` always commits, so `save` and `delete` must work even in a process
that only searches.

### Why `BlobIndex` is asynchronous

The engine calls the store from its own threads — segment writers, merges,
lazy loads — while a JavaScript callback can only run on the JavaScript
thread. If that thread were blocked inside a synchronous call such as
`index.commit()`, the callbacks it waits for could never run. So every
`BlobIndex` method runs its work on the Node.js thread pool and returns a
Promise; while you `await` it, the event loop is free and the store
callbacks are dispatched through it (one `ThreadsafeFunction` per method;
the engine thread blocks on a channel until the callback answered).

The corollary: **the JavaScript thread must stay free while a `BlobIndex`
call is pending.** Never `await` a `BlobIndex` call from inside a store
callback, and do not block the event loop with synchronous work (a
`readFileSync` inside `load` is fine; a `while` loop waiting for something
is not). Store callbacks that return Promises are awaited normally.

### Example: a Map

```javascript
const { BlobIndex } = require('lucivy');

const blobs = new Map();
const key = (indexName, fileName) => `${indexName}|${fileName}`;
const store = {
    load: (i, f) => blobs.get(key(i, f)) ?? null,
    save: (i, f, data) => { blobs.set(key(i, f), Buffer.from(data)); },
    delete: (i, f) => { blobs.delete(key(i, f)); },
    exists: (i, f) => blobs.has(key(i, f)),
    list: (i) => [...blobs.keys()].filter(k => k.startsWith(i + '|')).map(k => k.slice(i.length + 1)),
};

const index = await BlobIndex.create(store, 'articles', [
    { name: 'body', type: 'text', stored: true },
], { shards: 2 });
await index.addMany([{ docId: 1, body: 'kmalloc' }, { docId: 2, body: 'spin_lock_init' }]);
await index.commit();
const hits = await index.search('kmalloc', { highlights: true });
await index.close();

// Later, elsewhere — same blobs, same answers:
const again = await BlobIndex.open(store, 'articles');
await again.search({ type: 'contains', field: 'body', value: 'lock' });
await again.close();
```

With a database, `save` is an upsert, `load` a select, and `commit()` can be
wrapped in a transaction by the store itself.

### API

```javascript
BlobIndex.create(store, indexName, fields, options?)   // → Promise<BlobIndex>
BlobIndex.open(store, indexName, options?)             // → Promise<BlobIndex>
// options: { cacheDir?: string, lazy?: boolean, shards?: number (create only) }

await index.add(docId, fields);       await index.addMany(docs);
await index.delete(docId);            await index.update(docId, fields);
await index.commit();                 await index.search(query, options);
await index.queryWarnings(query);     await index.numDocs();
await index.compact(maxDocs?);        await index.waitMergesQuiet();
await index.indexBytes();             await index.close();
await index.dropIndex();
index.indexName; index.numShards; index.schema   // synchronous getters
```

Queries, options and results are exactly those of `Index`. `dropIndex()`
closes the index, then deletes every blob it owns: it lists and deletes the
`Lucivy_<name>/shard_<i>` namespaces and the `<name>` namespace through
`store.list()` / `store.delete()`; other indexes in the same store are
untouched. As with `Index`, the instance is consumed — every later call
rejects.

### Cache directory and lazy loading

`cacheDir` (default: `lucivy_blob_cache` under the OS temp dir) receives a
`<pid>/<namespace>_<n>/` directory per shard, filled from the store and
removed when the index is dropped from memory. It is disposable; it must be
on a local filesystem that supports mmap.

By default every blob is pulled at `open()`. With `lazy: true` — worth it
only when the store implements `blobLen` and `loadRange` — `open()` reads
the metadata files and probes segment footers with small `loadRange` calls;
each other file is loaded whole the first time a query needs more than a few
kilobytes of it. Searches give the same answers either way; the first query
pays for what it touches.

### Close before exit

`close()` waits for background merges, commits, releases the lock and
guarantees that nothing touches the store afterwards — call it before tearing
down a database connection and before the process exits. A store that stays
idle does not keep the process alive; if an index is garbage-collected
without `close()`, a flush that would have needed the JavaScript thread is
refused rather than deadlocked, and only the last uncommitted changes are
lost.

A store error — thrown or rejected, from a segment write in the background
or from `meta.json` at the commit point — comes back as the rejection of the
`BlobIndex` call that needed it, with the store's own message.

## License

MIT
