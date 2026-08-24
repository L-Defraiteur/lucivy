// Live smoke test of the Node binding: queryWarnings + search + highlights.
//
// Build and run (node only loads native addons with a .node extension, and
// require() resolves relative paths from this file — hand it an absolute path):
//     cd bindings/nodejs && cargo build --release
//     mkdir -p /tmp/lucivy_node && cp ../../target/release/liblucivy_napi.so /tmp/lucivy_node/lucivy.node
//     node tests/smoke_warnings.mjs /tmp/lucivy_node/lucivy.node
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
  [{ type: 'parse', field: 'body', value: 'kmalloc' }, 0],
  [{ type: 'parse', field: 'body', value: 'kmalloc spin' }, 1],
  [{ type: 'parse', field: 'body', value: 'kmalloc AND NOT vfree' }, 1],
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
for (const value of ['kmalloc spin', 'kmalloc AND NOT vfree', '"spin_lock" -vfree']) {
  const p = idx.search({ type: 'parse', field: 'body', value }, { highlights: true });
  console.log('parse', JSON.stringify(value), '->', p.length, 'hit(s), highlights:', JSON.stringify(p[0]?.highlights));
  if (!p.length || !p[0].highlights) { console.log('  EXPECTED one hit with highlights'); fails++; }
}
const none = idx.search({ type: 'parse', field: 'body', value: 'kmalloc AND vfree' }, { highlights: true });
console.log('parse AND with absent word ->', none.length, 'hit(s)');
if (none.length) { console.log('  EXPECTED no hit'); fails++; }
console.log('FAILS', fails);
process.exit(fails ? 1 : 0);
