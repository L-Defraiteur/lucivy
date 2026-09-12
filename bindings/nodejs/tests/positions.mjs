// `Index.create(path, fields, shards, sharedDictionary, derivedInRam,
// dictionaryWait, false)` — postings without positions, every match verified
// on the stored text — answers exactly like the default index: same
// documents, same scores, same highlights, over several commits on two
// shards, and after a close / open; no `.posmap` / `.word_pos_map` /
// `.sibling_v3` is on disk; the options object (`{ shards, positions }`)
// builds the same index; and the options it cannot live with are refused.
//
// Build and run:
//     cd bindings/nodejs && npm run build
//     node tests/positions.mjs
import { createRequire } from 'module';
const require = createRequire(import.meta.url);
const { Index } = require('../index.js');
import { mkdtempSync, readdirSync } from 'fs';
import { tmpdir } from 'os';
import { join } from 'path';

let fails = 0;
function check(cond, label) {
  console.log((cond ? 'ok   ' : 'FAIL ') + label);
  if (!cond) fails++;
}
// The directory's entries, recursively, without a stat per entry: the
// background dictionary fold creates and renames temporary files while this
// runs, and a stat on a file listed then gone threw ENOENT (one CI run in four).
function walk(dir) {
  const out = [];
  for (const e of readdirSync(dir, { withFileTypes: true })) {
    if (e.isDirectory()) out.push(...walk(join(dir, e.name))); else out.push(e.name);
  }
  return out;
}

const dir = mkdtempSync(join(tmpdir(), 'lucivy-positions-'));
const fields = [{ name: 'body', type: 'text', stored: true }];
const plain = Index.create(join(dir, 'plain'), fields, 2); // the default: positions
const leanPath = join(dir, 'lean');
const lean = Index.create(leanPath, fields, 2, true, false, true, false);

const words = ['kmalloc', 'spin_lock_init', 'vfree', 'mutex_lock', 'schedule', 'pthread_mutex_lock'];
for (const idx of [plain, lean]) {
  let id = 1;
  for (let round = 0; round < 4; round++) {
    for (const w of words) {
      idx.add(id, { body: `round ${round} calls ${w} and returns ${w.length}; déjà ${w.toUpperCase()}` });
      id++;
    }
    idx.commit();
  }
}
check(plain.numDocs === lean.numDocs, `numDocs ${lean.numDocs}`);
const names = walk(leanPath);
check(names.some(n => n.endsWith('.sfxpost')), 'segments were written');
check(!names.some(n => /\.(posmap|word_pos_map|sibling_v3)$/.test(n)), 'no position sidecar on disk');

const queries = [
  { type: 'contains', field: 'body', value: 'mutex' },
  { type: 'contains', field: 'body', value: 'spin_lock_init', strict_separators: true },
  { type: 'contains', field: 'body', value: 'spin lock', strict_separators: false },
  { type: 'contains', field: 'body', value: 'DÉJÀ' },
  { type: 'fuzzy', field: 'body', value: 'kmaloc', distance: 1 },
  { type: 'fuzzy', field: 'body', value: 'mutx_lock', distance: 1 },
  { type: 'regex', field: 'body', value: 'mutex_[a-z]+' },
  { type: 'parse', field: 'body', value: 'kmalloc AND NOT vfree' },
];
// Ties (equal scores) come back in segment order, which depends on when the
// background merges landed: compare the answers sorted by document.
const answer = (idx, q) => JSON.stringify(idx.search(q, { limit: 100, highlights: true })
  .map(r => [r.docId, Math.round(r.score * 1e4) / 1e4, r.highlights])
  .sort((a, b) => a[0] - b[0]));
for (const q of queries) {
  const a = answer(plain, q);
  check(a !== '[]', `the default index finds ${JSON.stringify(q)}`);
  check(answer(lean, q) === a, `same answer for ${JSON.stringify(q)}`);
}
lean.close();
const reopened = Index.open(leanPath);
for (const q of queries) {
  check(answer(reopened, q) === answer(plain, q), `same answer after reopen for ${q.value}`);
}

// The options as one object: the same index as the positional arguments.
const objPath = join(dir, 'object');
const viaObject = Index.create(objPath, fields, { shards: 2, positions: false });
let oid = 1;
for (let round = 0; round < 4; round++) {
  for (const w of words) {
    viaObject.add(oid, { body: `round ${round} calls ${w} and returns ${w.length}; déjà ${w.toUpperCase()}` });
    oid++;
  }
  viaObject.commit();
}
check(!walk(objPath).some(n => /\.(posmap|word_pos_map|sibling_v3)$/.test(n)), 'options object: no position sidecar on disk');
for (const q of queries) {
  check(answer(viaObject, q) === answer(plain, q), `options object: same answer for ${q.value}`);
}
// `stored` left out means stored: accepted.
const implicit = Index.create(join(dir, 'implicit'), [{ name: 'body', type: 'text' }], { positions: false });
check(implicit.numDocs === 0, 'a text field without `stored` is stored: accepted');

// Refused: with derivedInRam (nothing to rebuild), on a text field marked
// `stored: false`, and options given both as an object and as arguments.
let refused = '';
try { Index.create(join(dir, 'both'), fields, 1, true, true, true, false); } catch (e) { refused = String(e); }
check(refused.includes('derived_in_ram'), `refused with derivedInRam: ${refused}`);
refused = '';
try { Index.create(join(dir, 'unstored'), [{ name: 'body', type: 'text', stored: false }], { positions: false }); } catch (e) { refused = String(e); }
check(refused.includes('must be stored'), `refused on an unstored field: ${refused}`);
refused = '';
try { Index.create(join(dir, 'mixed'), fields, { positions: false }, true); } catch (e) { refused = String(e); }
check(refused.includes('not both'), `refused: object and arguments together: ${refused}`);

console.log('FAILS', fails);
process.exit(fails ? 1 : 0);
