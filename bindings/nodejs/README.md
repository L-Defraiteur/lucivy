# lucivy

Fast BM25 full-text search for Node.js — with substring matching, fuzzy search, regex, and highlights. Powered by Rust via napi-rs.

## Install

```bash
npm install lucivy
```

## Quick start

```javascript
const { Index } = require('lucivy');

const index = Index.create('./my_index', [
    { name: 'title', type: 'text' },
    { name: 'body', type: 'text' },
]);

index.add(1, { title: 'Rust Programming', body: 'Systems programming with memory safety' });
index.add(2, { title: 'Python Guide', body: 'Data science and web development' });
index.commit();

let results = index.search('programming', { highlights: true });
for (const r of results) {
    console.log(r.docId, r.score, r.highlights);
}
```

## API

### Create / open

```javascript
const index = Index.create('./my_index', [
    { name: 'title', type: 'text' },
    { name: 'body',  type: 'text' },
    { name: 'tag',   type: 'keyword' },
    { name: 'year',  type: 'i64', indexed: true, fast: true },
]);

const index2 = Index.open('./my_index');
```

### Add / update / delete

```javascript
index.add(1, { title: 'Hello', body: 'World' });
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

// Options
results = index.search('rust', { limit: 20, highlights: true, allowedIds: [1, 3] });

// Retrieve stored field values with results
results = index.search('rust', { fields: true });
for (const r of results) {
    console.log(r.docId, r.fields.title, r.fields.body);
}
```

#### contains — substring, fuzzy, regex (cross-token)

Searches **stored text**, not individual tokens. Handles multi-word phrases, substrings, typos, and regex across token boundaries.

```javascript
// Substring — matches "programming", "programmer", etc.
index.search({ type: 'contains', field: 'body', value: 'program' });

// Multi-word phrase
index.search({ type: 'contains', field: 'body', value: 'memory safety' });

// Fuzzy (catches typos, distance=1 by default)
index.search({ type: 'contains', field: 'body', value: 'programing languag', distance: 1 });

// Regex on stored text
index.search({ type: 'contains', field: 'body', value: 'program.*language', regex: true });
```

#### contains_split — one word = one contains query, OR'd together

```javascript
// "rust safety" → contains("rust") OR contains("safety") on body
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

#### keyword / range — for non-text fields

```javascript
index.search({ type: 'keyword', field: 'tag', value: 'rust' });
index.search({
    type: 'contains', field: 'body', value: 'programming',
    filters: [{ field: 'year', op: 'gte', value: 2023 }],
});
```

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

### Properties

```javascript
index.numDocs   // number of documents
index.path      // index directory path
index.schema    // array of field definitions
```

## License

MIT
