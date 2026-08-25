// Live test of BlobIndex: an index whose storage is a JavaScript object
// (the "bring your own storage" protocol over lucivy-core's ACID blob storage).
//
// Build and run:
//     cd bindings/nodejs && npm run build
//     node tests/blob_store.mjs
import { createRequire } from 'module';
const require = createRequire(import.meta.url);
const { BlobIndex } = require('../index.js');
import { mkdtempSync, writeFileSync, readFileSync, existsSync } from 'fs';
import { execFileSync } from 'child_process';
import { tmpdir } from 'os';
import { join } from 'path';

let fails = 0;
function check(cond, label) {
  console.log((cond ? 'ok   ' : 'FAIL ') + label);
  if (!cond) fails++;
}
async function rejects(promise, label) {
  try { await promise; check(false, label + ' (did not reject)'); return null; }
  catch (e) { check(true, label + ' -> ' + String(e.message).split('\n')[0]); return e; }
}
// Every awaited call is raced against a timeout: a hang is the failure mode
// this API exists to avoid, so it must show up as a failure, not a stall.
function withTimeout(promise, ms, label) {
  let timer;
  const guard = new Promise((_, reject) => {
    timer = setTimeout(() => reject(new Error(`timeout after ${ms}ms: ${label}`)), ms);
  });
  return Promise.race([promise, guard]).finally(() => clearTimeout(timer));
}
const T = (p, label) => withTimeout(p, 30000, label);

// ── A Map-backed store, with call counters ──
function mapStore(map = new Map(), opts = {}) {
  const calls = { load: 0, save: 0, delete: 0, exists: 0, list: 0, blobLen: 0, loadRange: 0 };
  const loaded = [];
  const key = (i, f) => `${i}\u0000${f}`;
  const store = {
    map, calls, loaded,
    load(indexName, fileName) {
      calls.load++; loaded.push(`${indexName}/${fileName}`);
      const v = map.get(key(indexName, fileName));
      return v === undefined ? null : v;
    },
    save(indexName, fileName, data) {
      calls.save++;
      if (opts.failSaves && opts.failSaves()) throw new Error('disk full (simulated)');
      map.set(key(indexName, fileName), Buffer.from(data));
    },
    delete(indexName, fileName) { calls.delete++; map.delete(key(indexName, fileName)); },
    exists(indexName, fileName) { calls.exists++; return map.has(key(indexName, fileName)); },
    list(indexName) {
      calls.list++;
      const prefix = indexName + '\u0000';
      return [...map.keys()].filter(k => k.startsWith(prefix)).map(k => k.slice(prefix.length));
    },
  };
  if (opts.lazy) {
    store.blobLen = (indexName, fileName) => {
      calls.blobLen++;
      const v = map.get(key(indexName, fileName));
      return v === undefined ? null : v.length;
    };
    store.loadRange = (indexName, fileName, offset, length) => {
      calls.loadRange++;
      const v = map.get(key(indexName, fileName));
      return v === undefined ? null : v.subarray(offset, offset + length);
    };
  }
  return store;
}

const fields = [{ name: 'body', type: 'text', stored: true }];
const docs = [];
for (let i = 1; i <= 40; i++) {
  docs.push({ docId: i, body: `doc ${i}: std::shared_ptr<binder::Expression> expr_${i}; kmalloc(sizeof(x)); spin_lock_init(&l${i});` });
}
const q = { type: 'contains', field: 'body', value: 'spin_lock_init' };
const key = (rs) => rs.map(r => `${r.docId}:${r.score.toFixed(4)}`).sort().join(',');
const tmp = mkdtempSync(join(tmpdir(), 'lucivy-blob-'));
const cacheDir = join(tmp, 'cache');

// ── 1. Map store: create, index, commit, search ──
console.log('-- Map store');
const shared = new Map();
const s1 = mapStore(shared);
const idx = await T(BlobIndex.create(s1, 'demo', fields, { cacheDir, shards: 2 }), 'create');
check(idx.indexName === 'demo', 'indexName getter');
check(idx.numShards === 2, `numShards === 2 (got ${idx.numShards})`);
check(idx.schema.length === 1 && idx.schema[0].name === 'body', 'schema getter');
await T(idx.addMany(docs), 'addMany');
await T(idx.add(41, { body: 'one more: kmalloc later' }), 'add');
await T(idx.commit(), 'commit');
check(await T(idx.numDocs(), 'numDocs') === 41, 'numDocs() === 41');
const hits = await T(idx.search(q, { limit: 100, highlights: true, fields: true }), 'search');
check(hits.length === 40, `search spin_lock_init -> ${hits.length} hits`);
check(hits[0].fields && typeof hits[0].fields.body === 'string', 'stored fields returned');
check(hits[0].highlights && hits[0].highlights.body.length > 0, 'highlights returned');
const strQ = await T(idx.search('kmalloc', { limit: 100 }), 'string search');
check(strQ.length === 41, `string query -> ${strQ.length} hits`);
const warn = await T(idx.queryWarnings({ type: 'contains', field: 'body', value: '__init' }), 'queryWarnings');
check(Array.isArray(warn) && warn.length === 1, `queryWarnings -> ${JSON.stringify(warn)}`);
await T(idx.delete(41), 'delete');
await T(idx.update(1, { body: 'doc 1 rewritten: spin_lock_init(&z1)' }), 'update');
await T(idx.commit(), 'commit 2');
check(await T(idx.numDocs(), 'numDocs') === 40, 'numDocs() === 40 after delete + update');
const merges = await T(idx.compact(), 'compact');
check(Number.isInteger(merges) && merges >= 0, `compact() -> ${merges}`);
const quiet = await T(idx.waitMergesQuiet(), 'waitMergesQuiet');
check(Number.isInteger(quiet) && quiet >= 0, `waitMergesQuiet() -> ${quiet}`);
const bytes = await T(idx.indexBytes(), 'indexBytes');
check(bytes > 0, `indexBytes() -> ${bytes}`);
const before = await T(idx.search(q, { limit: 100 }), 'search before close');
check(before.length === 40, `search after compact -> ${before.length} hits`);
await T(idx.close(), 'close');
const savesAtClose = s1.calls.save;
check(shared.size > 0, `store holds ${shared.size} blobs after close`);
check([...shared.keys()].some(k => k.startsWith('Lucivy_demo/shard_0\u0000')), 'shard blobs live under Lucivy_demo/shard_0');
check([...shared.keys()].some(k => k.startsWith('Lucivy_demo/shard_1\u0000')), 'shard blobs live under Lucivy_demo/shard_1');
check(shared.has('demo\u0000_shard_config.json'), 'root file demo/_shard_config.json');
check(![...shared.keys()].some(k => k.endsWith('.lock')), 'no lock files in the store');
// Nothing may touch the store after close(): this is what lets a caller tear
// down the database behind it.
await new Promise(r => setTimeout(r, 300));
check(s1.calls.save === savesAtClose, 'no store.save after close()');

// ── 2. Reopen from the same Map ──
console.log('-- reopen');
const s2 = mapStore(shared);
const re = await T(BlobIndex.open(s2, 'demo', { cacheDir }), 'open');
check(re.numShards === 2, 'reopened numShards === 2');
check(await T(re.numDocs(), 'numDocs') === 40, 'reopened numDocs() === 40');
const after = await T(re.search(q, { limit: 100, highlights: true, fields: true }), 'search reopened');
check(key(after) === key(before), `reopened search identical (${after.length} hits, same ids and scores)`);
const src = await T(idx.search(q, { limit: 100, highlights: true, fields: true }), 'search closed source').catch(() => null);
if (src) check(JSON.stringify(after[0].highlights) === JSON.stringify(src[0].highlights), 'reopened highlights match');
// It keeps writing.
await T(re.add(100, { body: 'fresh after reopen kmalloc' }), 'add after reopen');
await T(re.commit(), 'commit after reopen');
const fresh = await T(re.search({ type: 'contains', field: 'body', value: 'fresh after reopen' }, { limit: 10 }), 'search fresh');
check(fresh.length === 1 && fresh[0].docId === 100, 'document added after reopen is found');
await T(re.close(), 'close reopened');

// ── 3. Promise-returning store (every method async, some delayed) ──
console.log('-- async store');
const asyncMap = new Map();
const base = mapStore(asyncMap);
const delayed = (v) => new Promise(r => setTimeout(() => r(v), 1));
const asyncStore = {
  async load(i, f) { return delayed(base.load(i, f)); },
  async save(i, f, d) { await delayed(); base.save(i, f, d); },
  async delete(i, f) { base.delete(i, f); },
  async exists(i, f) { return base.exists(i, f); },
  async list(i) { return delayed(base.list(i)); },
};
const ai = await T(BlobIndex.create(asyncStore, 'async_demo', fields, { cacheDir }), 'create async');
await T(ai.addMany(docs.slice(0, 10)), 'addMany async');
await T(ai.commit(), 'commit async');
const ah = await T(ai.search(q, { limit: 100 }), 'search async');
check(ah.length === 10, `async store: ${ah.length} hits`);
await T(ai.close(), 'close async');
const ai2 = await T(BlobIndex.open(asyncStore, 'async_demo', { cacheDir }), 'open async');
check((await T(ai2.search(q, { limit: 100 }), 'search async reopened')).length === 10, 'async store reopen: 10 hits');
await T(ai2.close(), 'close async 2');

// ── 4. A store that throws: the error comes back, nothing hangs ──
console.log('-- throwing store');
// (a) every save fails: segment files are written by the background segment
// finalizer; its error travels back through the actor reply and commit()
// rejects with the store's own message.
let failing = null;
const bad = mapStore(new Map(), { failSaves: () => failing && failing() });
const bi = await T(BlobIndex.create(bad, 'bad', fields, { cacheDir }), 'create bad');
await T(bi.add(1, { body: 'kmalloc' }), 'add bad');
failing = () => true;
const errA = await rejects(T(bi.commit(), 'commit on throwing store'), 'commit() rejects when every store.save throws');
check(errA && /disk full \(simulated\)/.test(errA.message), 'rejection from a segment write carries the thrown message');
failing = null;
// (b) only meta.json fails — the commit point, written by the committing
// thread itself: the thrown message comes back verbatim.
let metaFails = false;
const bad2 = mapStore(new Map(), { failSaves: () => metaFails && bad2.currentFile === 'meta.json' });
const bad2Save = bad2.save;
bad2.save = function (i, f, d) { bad2.currentFile = f; return bad2Save.call(this, i, f, d); };
const bi2 = await T(BlobIndex.create(bad2, 'bad2', fields, { cacheDir }), 'create bad2');
await T(bi2.add(1, { body: 'kmalloc' }), 'add bad2');
metaFails = true;
const err = await rejects(T(bi2.commit(), 'commit with meta.json failing'), 'commit() rejects when store.save(meta.json) throws');
check(err && /disk full \(simulated\)/.test(err.message), 'rejection carries the thrown message');
metaFails = false;
// A rejected Promise from the store is reported the same way.
const rejecting = {
  ...mapStore(new Map()),
  load: async () => { throw new Error('connection refused (simulated)'); },
};
const err2 = await rejects(T(BlobIndex.open(rejecting, 'nope', { cacheDir }), 'open on rejecting store'), 'open() rejects when store.load rejects');
check(err2 && /connection refused/.test(err2.message), 'rejection carries the async message');
// Opening a name that was never created is an error, not a hang.
await rejects(T(BlobIndex.open(mapStore(new Map()), 'missing', { cacheDir }), 'open missing'), 'open() of an unknown index rejects');
// A malformed store object is refused up front, synchronously (argument error).
try { BlobIndex.create({ load() {} }, 'x', fields, { cacheDir }); check(false, 'create() with a store missing methods throws'); }
catch (e) { check(/store\.save must be a function/.test(e.message), 'create() with a store missing methods throws -> ' + e.message); }

// ── 5. lazy: true with blobLen / loadRange ──
console.log('-- lazy');
const lazyMap = new Map();
{
  const s = mapStore(lazyMap);
  const li = await T(BlobIndex.create(s, 'lazy_demo', fields, { cacheDir, shards: 2 }), 'create lazy source');
  await T(li.addMany(docs), 'addMany lazy source');
  await T(li.commit(), 'commit lazy source');
  await T(li.close(), 'close lazy source');
}
const eagerRef = await (async () => {
  const s = mapStore(lazyMap);
  const e = await T(BlobIndex.open(s, 'lazy_demo', { cacheDir }), 'open eager ref');
  const r = await T(e.search(q, { limit: 100, highlights: true }), 'search eager ref');
  await T(e.close(), 'close eager ref');
  return { hits: r, shardLoads: s.loaded.filter(n => n.startsWith('Lucivy_')).length };
})();
// What lazy guarantees (lucivy_core/tests/test_acid_blob_v3.rs, lazy_open_matches_eager):
// the open pulls less than half of the index — meta.json / .managed.json are
// read whole, segment files only get footer probes through loadRange — and a
// query then materializes what it touches, with identical answers.
const shardBytes = (names) => {
  let n = 0;
  for (const name of new Set(names)) {
    const i = name.lastIndexOf('/');
    const v = lazyMap.get(`${name.slice(0, i)}\u0000${name.slice(i + 1)}`);
    if (v) n += v.length;
  }
  return n;
};
const totalShardBytes = [...lazyMap.entries()].filter(([k]) => k.startsWith('Lucivy_lazy_demo/')).reduce((n, [, v]) => n + v.length, 0);
const ls = mapStore(lazyMap, { lazy: true });
const lz = await T(BlobIndex.open(ls, 'lazy_demo', { cacheDir, lazy: true }), 'open lazy');
const shardLoadedAtOpen = ls.loaded.filter(n => n.startsWith('Lucivy_'));
const bytesAtOpen = shardBytes(shardLoadedAtOpen);
check(ls.calls.list > 0 && ls.calls.blobLen > 0, `lazy open: list x${ls.calls.list}, blobLen x${ls.calls.blobLen}`);
check(bytesAtOpen < totalShardBytes / 2, `lazy open pulled ${bytesAtOpen} of ${totalShardBytes} shard bytes (< half)`);
check(shardLoadedAtOpen.length < eagerRef.shardLoads / 2, `lazy open: load x${shardLoadedAtOpen.length} (eager: x${eagerRef.shardLoads})`);
// Loaded whole at open: the files read with atomic_read (meta.json,
// .managed.json, _config.json) and the ones the segment open reads past the
// footer-probe budget (fast fields). Suffix and posting files are not.
const wholeAtOpen = [...new Set(shardLoadedAtOpen.map(n => n.split('/').pop()))];
check(!wholeAtOpen.some(f => /\.(sfx|sfxpost|termtexts|store|idx|term|pos)$/.test(f)), `no suffix / posting / docstore file loaded whole at open (${wholeAtOpen.join(', ')})`);
const rangesAtOpen = ls.calls.loadRange;
check(rangesAtOpen > 0, `segment open probed footers with loadRange (x${rangesAtOpen})`);
const lh = await T(lz.search(q, { limit: 100, highlights: true }), 'search lazy');
check(key(lh) === key(eagerRef.hits), `lazy search identical to eager (${lh.length} hits)`);
const shardLoadedAfterSearch = ls.loaded.filter(n => n.startsWith('Lucivy_'));
const bytesAfterSearch = shardBytes(shardLoadedAfterSearch);
check(bytesAfterSearch > bytesAtOpen, `search materialized what it touched (${bytesAfterSearch} bytes after ${bytesAtOpen} at open)`);
const shardFiles = [...lazyMap.keys()].filter(k => k.startsWith('Lucivy_lazy_demo/')).length;
check(new Set(shardLoadedAfterSearch).size < shardFiles, `search materialized ${new Set(shardLoadedAfterSearch).size} of ${shardFiles} shard blobs (not everything)`);
const fuzzy = await T(lz.search({ type: 'contains', field: 'body', value: 'kmaloc', distance: 1 }, { limit: 100 }), 'fuzzy lazy');
check(fuzzy.length === 40, `lazy fuzzy -> ${fuzzy.length} hits`);
await T(lz.close(), 'close lazy');

// ── 6. A JSON file on disk: another process can open it ──
console.log('-- JSON file store');
const jsonPath = join(tmp, 'store.json');
function jsonFileStore(path) {
  const data = existsSync(path) ? JSON.parse(readFileSync(path, 'utf8')) : {};
  const flush = () => writeFileSync(path, JSON.stringify(data));
  return {
    load: (i, f) => (data[i] && data[i][f] !== undefined ? Buffer.from(data[i][f], 'base64') : null),
    save: (i, f, d) => { (data[i] ??= {})[f] = Buffer.from(d).toString('base64'); flush(); },
    delete: (i, f) => { if (data[i]) { delete data[i][f]; flush(); } },
    exists: (i, f) => Boolean(data[i] && data[i][f] !== undefined),
    list: (i) => Object.keys(data[i] || {}),
  };
}
const js = await T(BlobIndex.create(jsonFileStore(jsonPath), 'ondisk', fields, { cacheDir }), 'create json');
await T(js.addMany(docs.slice(0, 12)), 'addMany json');
await T(js.commit(), 'commit json');
const jsHits = await T(js.search(q, { limit: 100 }), 'search json');
await T(js.close(), 'close json');
check(existsSync(jsonPath) && readFileSync(jsonPath, 'utf8').includes('meta.json'), 'store.json written with meta.json');
const child = `
  import { createRequire } from 'module';
  const require = createRequire(${JSON.stringify(import.meta.url)});
  const { BlobIndex } = require('../index.js');
  import { readFileSync } from 'fs';
  // A fresh in-memory copy of the file: nothing is shared with the parent
  // but the bytes on disk. Opening takes the writer lock through the store
  // (save then delete of .lucivy-writer.lock) and close() commits, so
  // save/delete must work even in a process that only reads.
  const data = JSON.parse(readFileSync(${JSON.stringify(jsonPath)}, 'utf8'));
  const written = [];
  const store = {
    load: (i, f) => (data[i] && data[i][f] !== undefined ? Buffer.from(data[i][f], 'base64') : null),
    save: (i, f, d) => { written.push(f); (data[i] ??= {})[f] = Buffer.from(d).toString('base64'); },
    delete: (i, f) => { if (data[i]) delete data[i][f]; },
    exists: (i, f) => Boolean(data[i] && data[i][f] !== undefined),
    list: (i) => Object.keys(data[i] || {}),
  };
  const ix = await BlobIndex.open(store, 'ondisk', { cacheDir: ${JSON.stringify(join(tmp, 'cache2'))} });
  const hits = await ix.search(${JSON.stringify(q)}, { limit: 100 });
  await ix.close();
  console.log(JSON.stringify({ hits: hits.map(r => [r.docId, r.score.toFixed(4)]).sort(), written }));
`;
const childOut = execFileSync(process.execPath, ['--input-type=module', '-e', child], { encoding: 'utf8', timeout: 60000 });
const childRes = JSON.parse(childOut.trim().split('\n').pop());
check(childRes.hits.length === 12, `second process opened the JSON store: ${childRes.hits.length} hits`);
check(JSON.stringify(childRes.hits) === JSON.stringify(jsHits.map(r => [r.docId, r.score.toFixed(4)]).sort()), 'second process: same ids and scores');
// open() takes the writer lock through the store and close() always commits:
// even a process that only reads writes lock files, meta.json and the router
// stats — but no segment file.
const bookkeeping = /(\.lock|^meta\.json|^\.managed\.json|^_shard_stats\.bin)$/;
check(childRes.written.every(f => bookkeeping.test(f)), `a reading process writes only bookkeeping files (${[...new Set(childRes.written)].join(', ')})`);

// ── 7. dropIndex on a blob store: every blob of the index is deleted ──
console.log('-- dropIndex');
const dm = new Map();
const ds = mapStore(dm);
const di = await T(BlobIndex.create(ds, 'todrop', fields, { cacheDir, shards: 2 }), 'create todrop');
await T(di.addMany(docs.slice(0, 8)), 'addMany todrop');
await T(di.commit(), 'commit todrop');
const other = mapStore(dm);
const keep = await T(BlobIndex.create(other, 'keep', fields, { cacheDir }), 'create keep');
await T(keep.add(1, { body: 'kmalloc' }), 'add keep');
await T(keep.commit(), 'commit keep');
await T(keep.close(), 'close keep');
const keepBlobs = [...dm.keys()].filter(k => k.startsWith('Lucivy_keep/') || k.startsWith('keep\u0000')).length;
check([...dm.keys()].some(k => k.startsWith('Lucivy_todrop/shard_1\u0000')), 'todrop blobs present before drop');
await T(di.dropIndex(), 'dropIndex');
const leftover = [...dm.keys()].filter(k => k.startsWith('Lucivy_todrop/') || k.startsWith('todrop\u0000'));
check(leftover.length === 0, `no blob left for todrop (${leftover.length} left)`);
check(ds.calls.delete > 0 && ds.calls.list > 0, `dropIndex went through store.list (x${ds.calls.list}) and store.delete (x${ds.calls.delete})`);
const keepAfter = [...dm.keys()].filter(k => k.startsWith('Lucivy_keep/') || k.startsWith('keep\u0000')).length;
check(keepAfter === keepBlobs, 'other index in the same store untouched');
await rejects(T(di.search(q), 'search after drop'), 'search after dropIndex rejects');
await rejects(T(di.commit(), 'commit after drop'), 'commit after dropIndex rejects');
await rejects(T(di.dropIndex(), 'second drop'), 'second dropIndex rejects');
await rejects(T(BlobIndex.open(mapStore(dm), 'todrop', { cacheDir }), 'open dropped'), 'open of a dropped index rejects');

console.log('FAILS', fails);
process.exit(fails ? 1 : 0);
