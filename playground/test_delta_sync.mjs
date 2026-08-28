#!/usr/bin/env node
// Incremental sync in the browser, end to end.
//
// The three entry points (shardVersions, exportShardedDelta,
// applyShardedDelta) were compiled into the wasm and listed in
// EXPORTED_FUNCTIONS long before anything called them, so "the symbol is
// exported" proves nothing here. What has to hold is the round trip: a client
// that is behind asks for what it misses, gets only that, applies it, and then
// answers the same as the server.
//
//   node test_delta_sync.mjs
//
// Serves this directory with COOP/COEP (SharedArrayBuffer needs them) and
// drives a real browser. Uses the system Chrome by default to avoid a
// download; PLAYWRIGHT_BROWSER=chromium uses playwright's own.

import http from 'node:http';
import fs from 'node:fs';
import path from 'node:path';

const ROOT = path.dirname(new URL(import.meta.url).pathname);
const PORT = 9879;

const MIME = {
  '.html': 'text/html', '.js': 'application/javascript',
  '.wasm': 'application/wasm', '.css': 'text/css',
};

const server = http.createServer((req, res) => {
  const url = new URL(req.url, `http://localhost:${PORT}`);
  const filePath = path.join(ROOT, url.pathname === '/' ? 'index.html' : url.pathname);
  if (!fs.existsSync(filePath) || fs.statSync(filePath).isDirectory()) {
    res.writeHead(404); res.end('Not found'); return;
  }
  res.writeHead(200, {
    'Content-Type': MIME[path.extname(filePath)] || 'application/octet-stream',
    'Cross-Origin-Opener-Policy': 'same-origin',
    'Cross-Origin-Embedder-Policy': 'require-corp',
  });
  fs.createReadStream(filePath).pipe(res);
});

// Everything below runs inside the page: the binding under test is the
// browser-side JS, so exercising it from node would test something else.
const SCENARIO = async () => {
  const { Lucivy } = await import('./js/lucivy.js');
  const out = [];
  const log = (m) => { out.push(m); console.log(m); };

  const lucivy = new Lucivy('./js/lucivy-worker.js');
  await lucivy.ready;

  const FIELDS = [{ name: 'body', type: 'text', stored: true }];

  // The server: three documents, committed.
  const server = await lucivy.create('/sync_server', { fields: FIELDS });
  for (const [id, body] of [[1, 'alpha mutex_lock'], [2, 'beta spin_lock'], [3, 'gamma printk']]) {
    await server.add(id, { body });
  }
  await server.commit();
  log(`server: ${await server.numDocs()} docs`);

  // The client starts from a full snapshot — the only thing possible before.
  const snap = await server.exportSnapshot();
  const client = await lucivy.importSnapshot(snap, '/sync_client');
  log(`client after snapshot: ${await client.numDocs()} docs (snapshot ${snap.length} bytes)`);
  if (await client.numDocs() !== 3) throw new Error('snapshot did not carry the documents');

  // What the client holds now. This is what makes the delta incremental.
  const versions = await client.shardVersions();
  if (!Array.isArray(versions)) throw new Error('shardVersions did not return a list');
  log(`client versions: ${versions.length} shard(s), keys ${Object.keys(versions[0] || {}).join(',')}`);

  // The server moves ahead.
  await server.add(4, { body: 'delta kmalloc' });
  await server.add(5, { body: 'delta kfree' });
  await server.commit();
  log(`server after two more: ${await server.numDocs()} docs`);

  // Only what moved.
  const delta = await server.exportShardedDelta(versions);
  if (!(delta instanceof Uint8Array) || delta.length === 0) throw new Error('empty delta');
  log(`delta: ${delta.length} bytes against a ${snap.length}-byte snapshot`);
  if (delta.length >= snap.length) {
    throw new Error(`delta (${delta.length}) is not smaller than the full snapshot (${snap.length}) — it is carrying more than what changed`);
  }

  await client.applyShardedDelta(delta);
  const after = await client.numDocs();
  log(`client after delta: ${after} docs`);
  if (after !== 5) throw new Error(`expected 5 documents after the delta, got ${after}`);

  // Same answer on both sides — the point of the whole exercise.
  const q = { type: 'contains', field: 'body', value: 'kmalloc' };
  const onServer = await server.search(q, { limit: 10 });
  const onClient = await client.search(q, { limit: 10 });
  log(`"kmalloc": server ${onServer.length} hit(s), client ${onClient.length}`);
  if (onClient.length !== onServer.length || onClient.length === 0) {
    throw new Error('the client does not answer like the server after the delta');
  }

  // A client already up to date must receive nothing to apply.
  const upToDate = await client.shardVersions();
  const empty = await server.exportShardedDelta(upToDate);
  log(`delta for an up-to-date client: ${empty.length} bytes`);

  return out;
};

server.listen(PORT, async () => {
  const { chromium } = await import('playwright');
  const channel = process.env.PLAYWRIGHT_BROWSER === 'chromium' ? undefined : 'chrome';
  const browser = await chromium.launch({ headless: true, channel });
  const page = await browser.newPage();
  page.on('console', m => console.log(`[browser] ${m.text()}`));
  page.on('pageerror', e => console.error(`[browser error] ${e.message}`));

  let failed = false;
  try {
    await page.goto(`http://localhost:${PORT}/`, { waitUntil: 'domcontentloaded' });
    await page.evaluate(`window.__scenario = ${SCENARIO.toString()}`);
    await page.evaluate('window.__scenario()', { timeout: 120000 });
    console.log('\n✓ DELTA SYNC OK — the client caught up without a full snapshot');
  } catch (e) {
    failed = true;
    console.error(`\n✗ FAILED: ${e.message}`);
  } finally {
    await browser.close();
    server.close();
    process.exit(failed ? 1 : 0);
  }
});
