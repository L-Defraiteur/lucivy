// Live test of the 3.0.0 additions to the Node binding: compact,
// waitMergesQuiet, indexBytes, openSnapshot / openSnapshotFrom, dropIndex.
//
// Build and run:
//     cd bindings/nodejs && npm run build
//     node tests/v3_api.mjs
import { createRequire } from 'module';
const require = createRequire(import.meta.url);
const { Index } = require('../index.js');
import { mkdtempSync, existsSync, writeFileSync, readdirSync } from 'fs';
import { tmpdir } from 'os';
import { join } from 'path';

let fails = 0;
function check(cond, label) {
  console.log((cond ? 'ok   ' : 'FAIL ') + label);
  if (!cond) fails++;
}
function throws(fn, label) {
  try { fn(); check(false, label + ' (did not throw)'); }
  catch (e) { check(true, label + ' -> ' + String(e.message).split('\n')[0]); }
}

const dir = mkdtempSync(join(tmpdir(), 'lucivy-v3-'));
const ixPath = join(dir, 'ix');
const idx = Index.create(ixPath, [{ name: 'body', type: 'text', stored: true }], 2);

// Several commits so that each shard holds more than one segment.
const words = ['kmalloc', 'spin_lock_init', 'vfree', 'mutex_lock', 'schedule'];
let id = 1;
for (let round = 0; round < 4; round++) {
  for (const w of words) {
    idx.add(id, { body: `round ${round} calls ${w} and returns` });
    id++;
  }
  idx.commit();
}
const total = id - 1;
check(idx.numDocs === total, `numDocs === ${total}`);

// ── indexBytes / waitMergesQuiet ──
const quiet = idx.waitMergesQuiet();
check(Number.isInteger(quiet) && quiet >= 0, `waitMergesQuiet() -> ${quiet}`);
const bytesBefore = idx.indexBytes();
check(typeof bytesBefore === 'number' && bytesBefore > 0, `indexBytes() -> ${bytesBefore} > 0`);

const q = { type: 'contains', field: 'body', value: 'spin_lock' };
const before = idx.search(q, { limit: 100 });
check(before.length === 4, `search spin_lock before compact: ${before.length} hits`);

// ── compact ──
const merges = idx.compact();
check(Number.isInteger(merges) && merges >= 0, `compact() -> ${merges} merges`);
check(idx.numDocs === total, `numDocs unchanged after compact (${idx.numDocs})`);
const after = idx.search(q, { limit: 100 });
check(after.length === before.length, `search spin_lock after compact: ${after.length} hits`);
const bytesAfter = idx.indexBytes();
check(bytesAfter > 0, `indexBytes() after compact -> ${bytesAfter}`);
const merges2 = idx.compact(5);
check(Number.isInteger(merges2) && merges2 >= 0, `compact(5) -> ${merges2} merges`);

// ── openSnapshot: same answers, read-only ──
const blob = idx.exportSnapshot();
check(blob.length > 0, `exportSnapshot -> ${blob.length} bytes`);
const snap = Index.openSnapshot(blob);
check(snap.path === '', `openSnapshot().path === '' (got ${JSON.stringify(snap.path)})`);
check(snap.numDocs === total, `openSnapshot().numDocs === ${total} (got ${snap.numDocs})`);
check(snap.numShards === 2, `openSnapshot().numShards === 2 (got ${snap.numShards})`);
const key = (rs) => rs.map(r => `${r.docId}:${r.score.toFixed(4)}`).sort().join(',');
const src = idx.search(q, { limit: 100, highlights: true, fields: true });
const viaSnap = snap.search(q, { limit: 100, highlights: true, fields: true });
check(key(src) === key(viaSnap), `snapshot search matches source (${viaSnap.length} hits, same ids and scores)`);
check(JSON.stringify(viaSnap[0].highlights) === JSON.stringify(src[0].highlights), 'snapshot highlights match');
check(viaSnap[0].fields.body === src[0].fields.body, 'snapshot stored fields match');
const parsed = snap.search({ type: 'parse', field: 'body', value: 'kmalloc AND NOT vfree' }, { limit: 100 });
check(parsed.length === 4, `snapshot parse query -> ${parsed.length} hits`);
const warn = snap.queryWarnings({ type: 'contains', field: 'body', value: '__init' });
check(Array.isArray(warn), `snapshot queryWarnings -> ${JSON.stringify(warn)}`);
throws(() => snap.add(999, { body: 'must not be indexed' }), 'snapshot add() throws');
throws(() => snap.addMany([{ docId: 998, body: 'nor this' }]), 'snapshot addMany() throws');
throws(() => snap.delete(1), 'snapshot delete() throws');
throws(() => snap.update(1, { body: 'nor this' }), 'snapshot update() throws');
throws(() => snap.commit(), 'snapshot commit() throws');
throws(() => snap.compact(), 'snapshot compact() throws');
throws(() => snap.exportSnapshot(), 'snapshot exportSnapshot() throws');
check(snap.numDocs === total, `snapshot numDocs still ${total} after refused writes`);

// ── openSnapshotFrom ──
const lucePath = join(dir, 'backup.luce');
writeFileSync(lucePath, blob);
const snap2 = Index.openSnapshotFrom(lucePath);
check(key(snap2.search(q, { limit: 100 })) === key(src), 'openSnapshotFrom search matches source');
check(snap2.schema.length === 1 && snap2.schema[0].name === 'body', 'openSnapshotFrom schema preserved');
snap2.close();
check(true, 'close() on a served snapshot is a no-op (did not throw)');

// ── dropIndex ──
check(existsSync(ixPath), 'index directory exists before dropIndex');
idx.dropIndex();
const left = existsSync(ixPath) ? readdirSync(ixPath) : null;
check(left === null || left.length === 0, `index directory gone after dropIndex (left: ${JSON.stringify(left)})`);
throws(() => idx.search(q), 'search after dropIndex throws');
throws(() => idx.numDocs, 'numDocs after dropIndex throws');
throws(() => idx.add(1, { body: 'x' }), 'add after dropIndex throws');
throws(() => idx.commit(), 'commit after dropIndex throws');
throws(() => idx.dropIndex(), 'second dropIndex throws');
throws(() => Index.open(ixPath), 'Index.open on dropped path throws');

// The snapshot never depended on the directory.
check(snap.search(q, { limit: 100 }).length === src.length, 'snapshot still searchable after source dropped');

console.log('FAILS', fails);
process.exit(fails ? 1 : 0);
