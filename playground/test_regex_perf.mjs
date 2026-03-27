#!/usr/bin/env node
// Test regex performance with WASM + .luce — no browser, no worker.
// Usage: node test_regex_perf.mjs

import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = dirname(fileURLToPath(import.meta.url));

// Load emscripten module
const createLucivy = (await import(join(__dirname, 'pkg/lucivy.js'))).default;

console.log('Initializing WASM module...');
const Module = await createLucivy({
  // Suppress emscripten stdout noise
  print: () => {},
  printErr: () => {},
});
console.log('WASM module ready.');

// Helper: call a C string-returning function
function callStr(fn, ...args) {
  const types = args.map(a => typeof a === 'number' ? 'number' : 'string');
  const ptr = Module.ccall(fn, 'number', types, args);
  return Module.UTF8ToString(ptr);
}

// Load .luce
const lucePath = join(__dirname, 'dataset.luce');
const luceData = readFileSync(lucePath);
console.log(`.luce loaded: ${(luceData.length / 1024).toFixed(0)} KB`);

// Import snapshot into WASM memory (async — ASYNCIFY)
const lucePtr = Module._malloc(luceData.length);
Module.HEAPU8.set(luceData, lucePtr);

const ctx = await Module.ccall(
  'lucivy_import_snapshot', 'number',
  ['number', 'number', 'string'],
  [lucePtr, luceData.length, '/test-regex'],
  { async: true }
);
Module._free(lucePtr);

if (!ctx) {
  console.error('Failed to import snapshot');
  process.exit(1);
}
console.log('Snapshot imported.\n');

// Search helper with timing (async for ASYNCIFY)
async function search(queryObj) {
  const json = JSON.stringify(queryObj);
  const t0 = performance.now();
  const resultPtr = await Module.ccall(
    'lucivy_search', 'number',
    ['number', 'string', 'number', 'number', 'number'],
    [ctx, json, 20, 0, 0],
    { async: true }
  );
  const elapsed = performance.now() - t0;
  const resultStr = Module.UTF8ToString(resultPtr);
  const parsed = JSON.parse(resultStr);
  const count = parsed.error ? 0 : (parsed.results?.length ?? 0);
  return { count, elapsed, error: parsed.error };
}

// Warmup
await search({ type: 'contains', field: 'content', value: 'test' });

console.log('=== Exact contains (baseline) ===');
for (const q of ['shard', 'incremental', 'flow', 'blob', 'rag3']) {
  const r = await search({ type: 'contains', field: 'content', value: q });
  console.log(`  "${q}" → ${r.count} results in ${r.elapsed.toFixed(1)}ms`);
}

console.log('\n=== Regex contains ===');
for (const pattern of [
  'shard[a-z]+',
  'incremental.sync',
  'flow.control',
  'blob.irectory',
  'rag3[a-z]+',
  'get.*element',
  '[a-z]+ment',
]) {
  const r = await search({ type: 'contains', field: 'content', value: pattern, regex: true });
  console.log(`  "${pattern}" → ${r.count} results in ${r.elapsed.toFixed(1)}ms${r.error ? ' ERROR: ' + r.error : ''}`);
}

console.log('\n=== Fuzzy contains d=1 ===');
for (const q of ['rak3weaver', 'weavr', 'shard']) {
  const r = await search({ type: 'contains', field: 'content', value: q, distance: 1 });
  console.log(`  "${q}" d=1 → ${r.count} results in ${r.elapsed.toFixed(1)}ms`);
}

// Run regex 5x to check consistency (cold vs warm)
console.log('\n=== Regex warmup check (shard[a-z]+) ===');
for (let i = 0; i < 5; i++) {
  const r = await search({ type: 'contains', field: 'content', value: 'shard[a-z]+', regex: true });
  console.log(`  run ${i + 1}: ${r.count} results in ${r.elapsed.toFixed(1)}ms`);
}

// Destroy
await Module.ccall('lucivy_destroy', null, ['number'], [ctx], { async: true });
console.log('\nDone.');
