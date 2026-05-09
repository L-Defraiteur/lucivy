import { createRequire } from 'module';
const require = createRequire(import.meta.url);
const { Index } = require('./index.js');

import { tmpdir } from 'os';
import { join } from 'path';
import { mkdirSync, rmSync } from 'fs';

const testDir = join(tmpdir(), 'lucivy_node_test_' + Date.now());
mkdirSync(testDir, { recursive: true });

try {
    // 1. Create index
    const idx = Index.create(testDir, [
        { name: 'title', type: 'text' },
        { name: 'body', type: 'text' },
    ]);

    console.log('Created index at:', idx.path);
    console.log('Schema:', idx.schema);

    // 2. Add documents
    idx.add(0, { title: 'Rust programming', body: 'Rust is a systems programming language' });
    idx.add(1, { title: 'Python scripting', body: 'Python is great for scripting and data science' });
    idx.add(2, { title: 'JavaScript everywhere', body: 'JavaScript runs in browsers and on servers with Node.js' });
    idx.commit();

    console.log('Num docs:', idx.numDocs);

    // 3. String search (contains_split on all text fields)
    console.log('\n--- String search: "rust programming" ---');
    const r1 = idx.search('rust programming');
    console.log(r1);

    // 4. Contains query with highlights
    console.log('\n--- Contains "script" with highlights ---');
    const r2 = idx.search(
        { type: 'contains', field: 'body', value: 'script' },
        { highlights: true }
    );
    console.log(JSON.stringify(r2, null, 2));

    // 5. Boolean query (composed of contains)
    console.log('\n--- Boolean: must contains "programming", must_not contains "python" ---');
    const r3 = idx.search({
        type: 'boolean',
        must: [{ type: 'contains', field: 'body', value: 'programming' }],
        must_not: [{ type: 'contains', field: 'body', value: 'python' }],
    });
    console.log(r3);

    // 6. Contains with fuzzy (distance option) — typo tolerance
    console.log('\n--- Contains fuzzy: "javascrip" (distance 2) ---');
    const r4 = idx.search({ type: 'contains', field: 'title', value: 'javascrip', distance: 2 });
    console.log(r4);

    // 7. Contains with regex option
    console.log('\n--- Contains regex: "program[a-z]+" ---');
    const r5 = idx.search({ type: 'contains', field: 'body', value: 'program[a-z]+', regex: true });
    console.log(r5);

    // 8. Contains multi-word (contains_split via string)
    console.log('\n--- Contains split: "systems language" ---');
    const r6 = idx.search('systems language');
    console.log(r6);

    // 9. Delete + update
    idx.delete(1);
    idx.update(2, { title: 'Node.js rocks', body: 'Node.js is JavaScript on the server side' });
    idx.commit();

    console.log('\nAfter delete+update, num docs:', idx.numDocs);
    const r7 = idx.search('node');
    console.log('Search "node":', r7);

    // 10. Search with fields — retrieve stored field values
    console.log('\n--- Search with fields: true ---');
    const r8 = idx.search('node', { fields: true });
    if (r8.length === 0) throw new Error('FAIL: fields search returned no results');
    if (!r8[0].fields) throw new Error('FAIL: result missing fields');
    if (!r8[0].fields.title) throw new Error('FAIL: result missing fields.title');
    console.log('fields.title:', r8[0].fields.title);
    console.log('fields.body:', r8[0].fields.body);

    // 11. Search with highlights + fields together
    console.log('\n--- Search with highlights + fields ---');
    const r9 = idx.search(
        { type: 'contains', field: 'body', value: 'server' },
        { highlights: true, fields: true }
    );
    if (r9.length === 0) throw new Error('FAIL: highlights+fields returned no results');
    if (!r9[0].highlights) throw new Error('FAIL: missing highlights');
    if (!r9[0].fields) throw new Error('FAIL: missing fields with highlights');
    console.log('highlights:', JSON.stringify(r9[0].highlights));
    console.log('fields.title:', r9[0].fields.title);

    // ── Snapshot tests ──────────────────────────────────────────────

    console.log('\n--- Snapshot: export/import roundtrip ---');
    const snapDir1 = join(testDir, 'snap_src');
    mkdirSync(snapDir1, { recursive: true });
    const si = Index.create(snapDir1, [
        { name: 'title', type: 'text' },
        { name: 'body', type: 'text' },
    ]);
    si.add(0, { title: 'Snapshot test', body: 'This is a snapshot roundtrip test' });
    si.add(1, { title: 'Second doc', body: 'Another document for testing' });
    si.commit();

    const blob = si.exportSnapshot();
    if (blob[0] !== 0x4C || blob[1] !== 0x55 || blob[2] !== 0x43 || blob[3] !== 0x45) {
        throw new Error('FAIL: bad LUCE magic');
    }
    console.log('Snapshot size:', blob.length, 'bytes');

    const snapDir2 = join(testDir, 'snap_dst');
    const si2 = Index.importSnapshot(blob, snapDir2);
    if (si2.numDocs !== 2) {
        throw new Error('FAIL: expected 2 docs after import, got ' + si2.numDocs);
    }
    const sr = si2.search('snapshot');
    if (sr.length === 0) {
        throw new Error('FAIL: search after import returned no results');
    }
    console.log('Import OK, numDocs:', si2.numDocs);

    // Export to file
    console.log('\n--- Snapshot: file export/import ---');
    const snapFile = join(testDir, 'test.luce');
    si.exportSnapshotTo(snapFile);
    const snapDir3 = join(testDir, 'snap_file_dst');
    const si3 = Index.importSnapshotFrom(snapFile, snapDir3);
    if (si3.numDocs !== 2) {
        throw new Error('FAIL: expected 2 docs from file import, got ' + si3.numDocs);
    }
    console.log('File import OK, numDocs:', si3.numDocs);

    // ── Sharded index ──
    console.log('\n--- Sharded index (2 shards) ---');
    const shardDir = join(testDir, 'sharded');
    mkdirSync(shardDir, { recursive: true });
    const sidx = Index.create(shardDir, [
        { name: 'title', type: 'text' },
        { name: 'body', type: 'text' },
    ], 2);
    for (let i = 0; i < 20; i++) {
        sidx.add(i, { title: `Doc ${i}`, body: `Content for document number ${i} about programming` });
    }
    sidx.commit();
    if (sidx.numDocs !== 20) throw new Error(`FAIL: expected 20 sharded docs, got ${sidx.numDocs}`);

    const sr1 = sidx.search('programming');
    if (sr1.length < 1) throw new Error('FAIL: sharded search found nothing');
    console.log('Sharded search OK:', sr1.length, 'results');

    // sharded snapshot round-trip
    const ssnap = sidx.exportSnapshot();
    const shardSnapDir = join(testDir, 'sharded_snap');
    const sidx2 = Index.importSnapshot(ssnap, shardSnapDir);
    if (sidx2.numDocs !== 20) throw new Error(`FAIL: sharded snapshot expected 20, got ${sidx2.numDocs}`);
    const sr2 = sidx2.search('programming');
    if (sr2.length < 1) throw new Error('FAIL: sharded snapshot search found nothing');
    console.log('Sharded snapshot OK:', sidx2.numDocs, 'docs');

    // ── Playground .luce import ──
    console.log('\n--- Playground .luce import ---');
    const lucePath = join(process.cwd(), 'playground', 'dataset.luce');
    const { existsSync, readFileSync } = await import('fs');
    if (existsSync(lucePath)) {
        const luceData = readFileSync(lucePath);
        const playDir = join(testDir, 'playground_import');
        const pidx = Index.importSnapshot(luceData, playDir);
        if (pidx.numDocs < 1) throw new Error('FAIL: playground snapshot has 0 docs');
        const pr = pidx.search({ type: 'contains', field: 'content', value: 'function' });
        if (pr.length < 1) throw new Error('FAIL: playground search found nothing');
        console.log(`Playground: ${pidx.numDocs} docs, search returned ${pr.length} results`);
    } else {
        console.log('SKIP: playground dataset.luce not found');
    }

    console.log('\nAll tests passed!');
} finally {
    rmSync(testDir, { recursive: true, force: true });
}
