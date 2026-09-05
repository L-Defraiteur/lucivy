// `Index.create(path, fields, shards, true)` — one dictionary per shard
// (`sfx_version` 4) — answers exactly like the default index: same
// documents, same scores, same highlights, over several commits on two
// shards, and after a close / open.
//
// Build and run:
//     cd bindings/nodejs && npm run build
//     node tests/shared_dictionary.mjs
import { createRequire } from 'module';
const require = createRequire(import.meta.url);
const { Index } = require('../index.js');
import { mkdtempSync } from 'fs';
import { tmpdir } from 'os';
import { join } from 'path';

let fails = 0;
function check(cond, label) {
  console.log((cond ? 'ok   ' : 'FAIL ') + label);
  if (!cond) fails++;
}

const dir = mkdtempSync(join(tmpdir(), 'lucivy-shared-dict-'));
const fields = [{ name: 'body', type: 'text', stored: true }];
const plain = Index.create(join(dir, 'plain'), fields, 2, false); // a suffix FST per segment: the reference
const sharedPath = join(dir, 'shared');
const shared = Index.create(sharedPath, fields, 2, true);

const words = ['kmalloc', 'spin_lock_init', 'vfree', 'mutex_lock', 'schedule', 'pthread_mutex_lock'];
for (const idx of [plain, shared]) {
  let id = 1;
  for (let round = 0; round < 4; round++) {
    for (const w of words) {
      idx.add(id, { body: `round ${round} calls ${w} and returns ${w.length}` });
      id++;
    }
    idx.commit();
  }
}
check(plain.numDocs === shared.numDocs, `numDocs ${shared.numDocs}`);

const queries = [
  { type: 'contains', field: 'body', value: 'mutex' },
  { type: 'contains', field: 'body', value: 'spin_lock_init', strict_separators: true },
  { type: 'fuzzy', field: 'body', value: 'kmaloc', distance: 1 },
  { type: 'regex', field: 'body', value: 'mutex_[a-z]+' },
  { type: 'parse', field: 'body', value: 'kmalloc AND NOT vfree' },
];
// Ties (equal scores) come back in segment order, which depends on when the
// background merges landed: compare the answers sorted by document.
const answer = (idx, q) => JSON.stringify(idx.search(q, { limit: 100, highlights: true })
  .map(r => [r.docId, Math.round(r.score * 1e4) / 1e4, r.highlights])
  .sort((a, b) => a[0] - b[0]));
for (const q of queries) {
  check(answer(shared, q) === answer(plain, q), `same answer for ${JSON.stringify(q)}`);
}
shared.close();
const reopened = Index.open(sharedPath);
for (const q of queries) {
  check(answer(reopened, q) === answer(plain, q), `same answer after reopen for ${q.value}`);
}

// `dictionaryWait: false` (a search does not wait for the background merge
// of the last commit's texts) is accepted and answers like the default.
const eager = Index.create(join(dir, 'eager'), fields, 2, true, false, false);
{
  let id = 1;
  for (let round = 0; round < 4; round++) {
    for (const w of words) { eager.add(id, { body: `round ${round} calls ${w} and returns ${w.length}` }); id++; }
    eager.commit();
  }
}
for (const q of queries) {
  check(answer(eager, q) === answer(plain, q), `dictionaryWait false: same answer for ${q.value}`);
}
eager.close();

console.log('FAILS', fails);
process.exit(fails ? 1 : 0);
