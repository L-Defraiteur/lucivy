// Quick test of lucivy-emscripten in Node.js
// Usage: node test-node.mjs

import createLucivy from './pkg/lucivy.js';

const Module = await createLucivy();
console.log('Module loaded');

// Helper to call C string functions
function callStr(fn, ...args) {
    const ptr = Module.ccall(fn, 'number', args.map(() => 'string'), args);
    return Module.UTF8ToString(ptr);
}

// Create index
const fieldsJson = JSON.stringify([{name: 'title', type: 'text'}, {name: 'body', type: 'text'}]);
const ctx = Module.ccall('lucivy_create', 'number', ['string', 'string', 'string'], ['/test', fieldsJson, 'english']);
if (!ctx) { console.error('FAIL: create returned null'); process.exit(1); }
console.log('Index created, ctx =', ctx);

// Add documents
function addDoc(docId, fields) {
    const ptr = Module.ccall('lucivy_add', 'number', ['number', 'number', 'string'], [ctx, docId, JSON.stringify(fields)]);
    const res = Module.UTF8ToString(ptr);
    if (res !== 'ok') throw new Error('add failed: ' + res);
}

addDoc(0, {title: 'Rust programming', body: 'Rust is a systems programming language'});
addDoc(1, {title: 'Python scripting', body: 'Python is great for data science'});
addDoc(2, {title: 'JavaScript', body: 'JS runs in browsers and servers'});
console.log('3 docs added');

// Commit
const commitRes = Module.UTF8ToString(
    Module.ccall('lucivy_commit', 'number', ['number'], [ctx])
);
if (commitRes !== 'ok') { console.error('FAIL: commit:', commitRes); process.exit(1); }
console.log('Committed');

// Num docs
const numDocs = Module.ccall('lucivy_num_docs', 'number', ['number'], [ctx]);
console.log('Num docs:', numDocs);
if (numDocs !== 3) { console.error('FAIL: expected 3 docs, got', numDocs); process.exit(1); }

// Search
const searchRes = Module.UTF8ToString(
    Module.ccall('lucivy_search', 'number', ['number', 'string', 'number', 'number'], [ctx, JSON.stringify('rust programming'), 10, 0])
);
console.log('Search "rust programming":', searchRes);
const results = JSON.parse(searchRes);
if (results.error) { console.error('FAIL: search error:', results.error); process.exit(1); }
if (results.length === 0) { console.error('FAIL: no results'); process.exit(1); }
if (results[0].docId !== 0) { console.error('FAIL: expected docId 0, got', results[0].docId); process.exit(1); }

// Search with highlights
const hlRes = Module.UTF8ToString(
    Module.ccall('lucivy_search', 'number', ['number', 'string', 'number', 'number'], [ctx, JSON.stringify({type: 'contains', field: 'body', value: 'science'}), 10, 1])
);
console.log('Search contains "science" with highlights:', hlRes);
const hlResults = JSON.parse(hlRes);
if (hlResults.error) { console.error('FAIL: highlight search error:', hlResults.error); process.exit(1); }

// Delete
Module.ccall('lucivy_remove', 'number', ['number', 'number'], [ctx, 1]);
const commitRes2 = Module.UTF8ToString(
    Module.ccall('lucivy_commit', 'number', ['number'], [ctx])
);
if (commitRes2 !== 'ok') { console.error('FAIL: commit2:', commitRes2); process.exit(1); }
const numDocs2 = Module.ccall('lucivy_num_docs', 'number', ['number'], [ctx]);
console.log('After delete, num docs:', numDocs2);
if (numDocs2 !== 2) { console.error('FAIL: expected 2 docs, got', numDocs2); process.exit(1); }

// addMany
const ctx2 = Module.ccall('lucivy_create', 'number', ['string', 'string', 'string'], ['/test2', fieldsJson, 'english']);
const addManyRes = Module.UTF8ToString(
    Module.ccall('lucivy_add_many', 'number', ['number', 'string'], [ctx2, JSON.stringify([
        { docId: 10, title: 'Go concurrency', body: 'Goroutines and channels' },
        { docId: 11, title: 'Zig lang', body: 'Zig is a systems language' },
    ])])
);
if (addManyRes !== 'ok') { console.error('FAIL: addMany:', addManyRes); process.exit(1); }
const commitRes3 = Module.UTF8ToString(
    Module.ccall('lucivy_commit', 'number', ['number'], [ctx2])
);
if (commitRes3 !== 'ok') { console.error('FAIL: commit3:', commitRes3); process.exit(1); }
const numDocs3 = Module.ccall('lucivy_num_docs', 'number', ['number'], [ctx2]);
console.log('addMany: num docs:', numDocs3);
if (numDocs3 !== 2) { console.error('FAIL: expected 2 docs from addMany, got', numDocs3); process.exit(1); }
Module.ccall('lucivy_destroy', 'void', ['number'], [ctx2]);

// ── Snapshot tests ──────────────────────────────────────────────
console.log('\n--- Snapshot: export/import roundtrip ---');

// Allocate u32 for out_len
const outLenPtr = Module._malloc(4);
const snapPtr = Module.ccall('lucivy_export_snapshot', 'number', ['number', 'number'], [ctx, outLenPtr]);
if (!snapPtr) { console.error('FAIL: export_snapshot returned null'); process.exit(1); }
const snapLen = Module.HEAPU32[outLenPtr >> 2];
Module._free(outLenPtr);
console.log('Snapshot size:', snapLen, 'bytes');

// Check LUCE magic
const magic = Module.HEAPU8.slice(snapPtr, snapPtr + 4);
if (magic[0] !== 0x4C || magic[1] !== 0x55 || magic[2] !== 0x43 || magic[3] !== 0x45) {
    console.error('FAIL: bad LUCE magic:', magic);
    process.exit(1);
}

// Copy blob (pointer valid until next export call)
const snapBlob = Module.HEAPU8.slice(snapPtr, snapPtr + snapLen);

// Import snapshot
const blobPtr = Module._malloc(snapLen);
Module.HEAPU8.set(snapBlob, blobPtr);
const ctx3 = Module.ccall('lucivy_import_snapshot', 'number', ['number', 'number', 'string'], [blobPtr, snapLen, '/test_snap']);
Module._free(blobPtr);
if (!ctx3) { console.error('FAIL: import_snapshot returned null'); process.exit(1); }

const snapNumDocs = Module.ccall('lucivy_num_docs', 'number', ['number'], [ctx3]);
console.log('Import numDocs:', snapNumDocs);
if (snapNumDocs !== 2) { console.error('FAIL: expected 2 docs after import, got', snapNumDocs); process.exit(1); }

// Search after import
const snapSearchRes = Module.UTF8ToString(
    Module.ccall('lucivy_search', 'number', ['number', 'string', 'number', 'number'], [ctx3, JSON.stringify('rust'), 10, 0])
);
const snapSearchParsed = JSON.parse(snapSearchRes);
if (snapSearchParsed.error) { console.error('FAIL: search after import error:', snapSearchParsed.error); process.exit(1); }
if (snapSearchParsed.length === 0) { console.error('FAIL: search after import returned no results'); process.exit(1); }
console.log('Search after import OK:', snapSearchRes);
Module.ccall('lucivy_destroy', 'void', ['number'], [ctx3]);

// Uncommitted export should fail
console.log('\n--- Snapshot: uncommitted should return null ---');
const ctx4 = Module.ccall('lucivy_create', 'number', ['string', 'string', 'string'], ['/test_uncommit', fieldsJson, '']);
Module.ccall('lucivy_add', 'number', ['number', 'number', 'string'], [ctx4, 0, JSON.stringify({title: 'uncommitted', body: 'test'})]);
const outLenPtr2 = Module._malloc(4);
const badSnapPtr = Module.ccall('lucivy_export_snapshot', 'number', ['number', 'number'], [ctx4, outLenPtr2]);
Module._free(outLenPtr2);
if (badSnapPtr) { console.error('FAIL: export_snapshot should have returned null for uncommitted'); process.exit(1); }
console.log('Correctly returned null for uncommitted export');
Module.ccall('lucivy_destroy', 'void', ['number'], [ctx4]);

// Destroy
Module.ccall('lucivy_destroy', 'void', ['number'], [ctx]);
console.log('\nALL TESTS PASSED');
