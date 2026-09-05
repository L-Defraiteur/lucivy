// `Index.create(path, fields, shards, sharedDictionary, true)` — the three
// derived sidecars of each segment are not written but rebuilt in RAM when
// the index opens — answers exactly like the default index: same documents,
// same scores, same highlights, over several commits on two shards, and
// after a close / open; and no `.posmap` / `.word_pos_map` / `.sibling_v3`
// is on disk.
//
// Build and run:
//     cd bindings/nodejs && npm run build
//     node tests/derived_in_ram.mjs
import { createRequire } from 'module';
const require = createRequire(import.meta.url);
const { Index } = require('../index.js');
import { mkdtempSync, readdirSync, statSync } from 'fs';
import { tmpdir } from 'os';
import { join } from 'path';

let fails = 0;
function check(cond, label) {
  console.log((cond ? 'ok   ' : 'FAIL ') + label);
  if (!cond) fails++;
}
function walk(dir) {
  const out = [];
  for (const n of readdirSync(dir)) {
    const p = join(dir, n);
    if (statSync(p).isDirectory()) out.push(...walk(p)); else out.push(n);
  }
  return out;
}

const dir = mkdtempSync(join(tmpdir(), 'lucivy-derived-'));
const fields = [{ name: 'body', type: 'text', stored: true }];
const plain = Index.create(join(dir, 'plain'), fields, 2);
const leanPath = join(dir, 'lean');
const lean = Index.create(leanPath, fields, 2, true, true);

const words = ['kmalloc', 'spin_lock_init', 'vfree', 'mutex_lock', 'schedule', 'pthread_mutex_lock'];
for (const idx of [plain, lean]) {
  let id = 1;
  for (let round = 0; round < 4; round++) {
    for (const w of words) {
      idx.add(id, { body: `round ${round} calls ${w} and returns ${w.length}` });
      id++;
    }
    idx.commit();
  }
}
check(plain.numDocs === lean.numDocs, `numDocs ${lean.numDocs}`);
const names = walk(leanPath);
check(names.some(n => n.endsWith('.sfxpost')), 'segments were written');
check(!names.some(n => /\.(posmap|word_pos_map|sibling_v3)$/.test(n)), 'no derived sidecar on disk');

const queries = [
  { type: 'contains', field: 'body', value: 'mutex' },
  { type: 'contains', field: 'body', value: 'spin_lock_init', strict_separators: true },
  { type: 'fuzzy', field: 'body', value: 'kmaloc', distance: 1 },
  { type: 'regex', field: 'body', value: 'mutex_[a-z]+' },
  { type: 'parse', field: 'body', value: 'kmalloc AND NOT vfree' },
];
const answer = (idx, q) => JSON.stringify(idx.search(q, { limit: 100, highlights: true })
  .map(r => [r.docId, Math.round(r.score * 1e4) / 1e4, r.highlights]));
for (const q of queries) {
  check(answer(lean, q) === answer(plain, q), `same answer for ${JSON.stringify(q)}`);
}
lean.close();
const reopened = Index.open(leanPath);
for (const q of queries) {
  check(answer(reopened, q) === answer(plain, q), `same answer after reopen for ${q.value}`);
}

console.log('FAILS', fails);
process.exit(fails ? 1 : 0);
