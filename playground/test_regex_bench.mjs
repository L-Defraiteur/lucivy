#!/usr/bin/env node
// Playwright benchmark: regex vs exact vs fuzzy performance in WASM.
// Runs in real Chromium with the actual playground + .luce dataset.
//
// Usage: node test_regex_bench.mjs
// Requires: npx playwright install chromium (or playwright already installed)

import http from 'node:http';
import fs from 'node:fs';
import path from 'node:path';

const ROOT = path.dirname(new URL(import.meta.url).pathname);
const PORT = 9878; // different from playground default to avoid conflicts

const MIME = {
  '.html': 'text/html',
  '.js': 'application/javascript',
  '.mjs': 'application/javascript',
  '.wasm': 'application/wasm',
  '.luce': 'application/octet-stream',
  '.css': 'text/css',
};

const server = http.createServer((req, res) => {
  const url = new URL(req.url, `http://localhost:${PORT}`);
  let filePath = path.join(ROOT, url.pathname === '/' ? 'index.html' : url.pathname);
  if (!fs.existsSync(filePath)) { res.writeHead(404); res.end('Not found'); return; }
  const ext = path.extname(filePath);
  const mime = MIME[ext] || 'application/octet-stream';
  res.writeHead(200, {
    'Content-Type': mime,
    'Cross-Origin-Opener-Policy': 'same-origin',
    'Cross-Origin-Embedder-Policy': 'require-corp',
  });
  fs.createReadStream(filePath).pipe(res);
});

server.listen(PORT, async () => {
  console.log(`Serving playground at http://localhost:${PORT}`);

  const { chromium } = await import('playwright');
  const browser = await chromium.launch({ headless: true });
  const page = await browser.newPage();

  // Capture ALL console logs — worker relays [rust] logs from WASM eprintln
  page.on('console', msg => {
    const text = msg.text();
    if (text.includes('regex-timer')) {
      console.log(text);
    }
  });
  page.on('pageerror', err => console.error(`[browser error] ${err.message}`));

  try {
    await page.goto(`http://localhost:${PORT}`, { waitUntil: 'domcontentloaded' });

    // Wait for index to be ready
    await page.waitForFunction(
      () => document.getElementById('status')?.textContent?.includes('documents indexed'),
      { timeout: 60000 }
    );
    const statusText = await page.textContent('#status');
    console.log(`\nIndex ready: ${statusText}\n`);

    // Benchmark via UI manipulation — same code path as manual testing.
    // Set mode, fill query, trigger search, read timing from results header.

    async function benchViaUI(query, mode, distance) {
      // Set mode
      await page.selectOption('#mode', mode || 'contains');
      // Enable highlights (same as manual testing)
      await page.check('#highlights');
      // Set distance
      if (distance !== undefined) {
        await page.fill('#distance', String(distance));
      } else {
        await page.fill('#distance', '0');
      }
      // Clear and fill query
      await page.fill('#query', '');
      await page.fill('#query', query);
      // Wait for results to appear (debounce + search)
      await page.waitForFunction(
        (q) => {
          const h = document.getElementById('resultsHeader')?.textContent || '';
          return h.includes('result') && !h.includes('Searching');
        },
        query,
        { timeout: 30000 }
      );
      // Wait for worker log poller to relay Rust logs (200ms poll interval)
      await page.waitForTimeout(500);
      // Read results header: "N results in X.Xms"
      const header = await page.textContent('#resultsHeader');
      return header;
    }

    const output = [];

    // Exact contains baseline
    output.push('=== Exact contains (baseline) ===');
    for (const q of ['shard', 'incremental', 'flow', 'rag3', 'mutex']) {
      const h = await benchViaUI(q, 'contains');
      output.push(`  "${q}": ${h}`);
    }

    // Regex contains
    output.push('\n=== Regex contains ===');
    for (const p of [
      'shard[a-z]+',
      'incremental.sync',
      'flow.control',
      'rag3[a-z]+',
      'blob.irectory',
    ]) {
      const h = await benchViaUI(p, 'regex');
      output.push(`  "${p}": ${h}`);
    }

    // Fuzzy contains d=1
    output.push('\n=== Fuzzy contains d=1 ===');
    for (const q of ['rak3weaver', 'weavr', 'shard']) {
      const h = await benchViaUI(q, 'contains', 1);
      output.push(`  "${q}" d=1: ${h}`);
    }

    console.log(output.join('\n'));

    console.log(results);
    console.log('\nBenchmark complete.');

  } catch (e) {
    console.error(`\nBenchmark FAILED: ${e.message}`);
    process.exitCode = 1;
  } finally {
    await browser.close();
    server.close();
  }
});
