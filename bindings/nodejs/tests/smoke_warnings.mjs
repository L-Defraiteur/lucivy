// Live smoke test of the Node binding: queryWarnings + search + highlights.
//
// Build and run:
//     cd bindings/nodejs && cargo build --release
//     node tests/smoke_warnings.mjs ../../target/release/liblucivy_napi.so
import { createRequire } from 'module';
const require = createRequire(import.meta.url);
const { Index } = require(process.argv[2]);
import { mkdtempSync } from 'fs'; import { tmpdir } from 'os'; import { join } from 'path';
const dir = mkdtempSync(join(tmpdir(), 'lucivy-'));
const idx = Index.create(join(dir, 'ix'), [{ name: 'body', type: 'text', stored: true }]);
idx.add(1, { body: 'the kmalloc call and spin_lock_init here' });
idx.commit();
const cases = [
  [{ type: 'contains', field: 'body', value: 'kmalloc' }, 0],
  [{ type: 'contains', field: 'body', value: '__init' }, 1],
  [{ type: 'regex', field: 'body', value: '[0-9]{8}' }, 1],
  [{ type: 'fuzzy', field: 'body', value: 'init' }, 1],
];
let fails = 0;
for (const [q, expect] of cases) {
  const w = idx.queryWarnings(q);
  console.log(JSON.stringify(q.value), '->', w);
  if (w.length !== expect) { console.log('  EXPECTED', expect); fails++; }
}
const r = idx.search({ type: 'contains', field: 'body', value: 'spin_lock' }, { highlights: true });
console.log('search spin_lock:', r.length, 'hit(s), highlights:', JSON.stringify(r[0]?.highlights));
if (!r.length) fails++;
console.log('FAILS', fails);
process.exit(fails ? 1 : 0);
