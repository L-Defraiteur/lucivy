# lucivy

Fast BM25 full-text search for Node.js — with substring matching, fuzzy search, regex, and highlights. Powered by Rust via napi-rs.

## Install

```bash
npm install lucivy
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
index.close()      // flush + release writer lock
```

## License

MIT
