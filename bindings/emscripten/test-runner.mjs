#!/usr/bin/env node
// Serves the emscripten binding with COOP/COEP headers and runs test.html via Playwright.
import http from 'node:http';
import fs from 'node:fs';
import path from 'node:path';
import { chromium } from 'playwright';

const DIR = path.dirname(new URL(import.meta.url).pathname);
const PORT = 9877;

const MIME = {
    '.html': 'text/html', '.js': 'application/javascript', '.mjs': 'application/javascript',
    '.wasm': 'application/wasm', '.css': 'text/css', '.json': 'application/json',
};

const server = http.createServer((req, res) => {
    const url = new URL(req.url, `http://localhost:${PORT}`);
    let filePath = path.join(DIR, decodeURIComponent(url.pathname));
    if (filePath.endsWith('/')) filePath += 'index.html';

    if (!fs.existsSync(filePath)) { res.writeHead(404); res.end('Not found'); return; }
    const ext = path.extname(filePath);
    const mime = MIME[ext] || 'application/octet-stream';

    // Required for SharedArrayBuffer (pthreads)
    res.setHeader('Cross-Origin-Opener-Policy', 'same-origin');
    res.setHeader('Cross-Origin-Embedder-Policy', 'require-corp');
    res.setHeader('Content-Type', mime);
    fs.createReadStream(filePath).pipe(res);
});

server.listen(PORT, async () => {
    console.log(`Serving on http://localhost:${PORT}`);
    let exitCode = 1;
    try {
        const browser = await chromium.launch();
        const page = await browser.newPage();
        page.on('console', msg => console.log(`[browser] ${msg.text()}`));
        page.on('pageerror', err => console.error(`[browser error] ${err}`));

        await page.goto(`http://localhost:${PORT}/test.html`, { waitUntil: 'domcontentloaded' });

        // Wait for __TEST_RESULT__ (max 60s)
        const result = await page.waitForFunction(() => window.__TEST_RESULT__, { timeout: 60000 });
        const value = await result.jsonValue();
        console.log(`\nResult: ${value}`);
        exitCode = value === 'PASS' ? 0 : 1;

        await browser.close();
    } catch (e) {
        console.error('Test runner error:', e.message);
    }
    server.close();
    process.exit(exitCode);
});
