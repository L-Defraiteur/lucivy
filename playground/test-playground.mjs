#!/usr/bin/env node
// Quick test: serve playground with COOP/COEP headers, load in Chromium, check startup + search
import http from 'node:http';
import fs from 'node:fs';
import path from 'node:path';

const ROOT = path.dirname(new URL(import.meta.url).pathname);
const PORT = 9877;

const MIME = {
  '.html': 'text/html',
  '.js': 'application/javascript',
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

  page.on('console', msg => console.log(`[browser] ${msg.text()}`));
  page.on('pageerror', err => console.error(`[browser error] ${err.message}`));

  try {
    await page.goto(`http://localhost:${PORT}`, { waitUntil: 'domcontentloaded' });

    // Wait for status to show "documents indexed" (startup complete)
    await page.waitForFunction(
      () => document.getElementById('status')?.textContent?.includes('documents indexed'),
      { timeout: 30000 }
    );
    const statusText = await page.textContent('#status');
    console.log(`✓ Startup OK: ${statusText}`);

    // Type a search query
    await page.fill('#query', 'search');
    // Wait for results to appear
    await page.waitForFunction(
      () => document.getElementById('resultsHeader')?.textContent?.includes('result'),
      { timeout: 10000 }
    );
    const resultsText = await page.textContent('#resultsHeader');
    console.log(`✓ Search OK: ${resultsText}`);

    // Check that result items exist
    const resultCount = await page.locator('.result').count();
    console.log(`✓ ${resultCount} result elements rendered`);

    if (resultCount === 0) throw new Error('No results rendered');

    // Check that file paths are shown (not just "doc #N")
    const firstPath = await page.locator('.result-path').first().textContent();
    console.log(`✓ First result path: ${firstPath}`);
    if (firstPath.startsWith('doc #')) throw new Error('Expected file path, got: ' + firstPath);

    // Check that snippets with <mark> highlights are shown
    const hasMarks = await page.locator('.result-body mark').count();
    console.log(`✓ ${hasMarks} highlight marks rendered`);

    console.log('\n✓ ALL PLAYGROUND TESTS PASSED');
  } catch (e) {
    console.error(`\n✗ TEST FAILED: ${e.message}`);
    process.exitCode = 1;
  } finally {
    await browser.close();
    server.close();
  }
});
