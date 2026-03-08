// Playwright test for lucivy-emscripten.
// Usage: node test-playwright.mjs
//
// Starts a local HTTP server with COOP/COEP headers (required for SharedArrayBuffer),
// opens test.html in Chromium, checks window.__TEST_RESULT__.

import { chromium } from 'playwright';
import { createServer } from 'http';
import { readFile } from 'fs/promises';
import { join, extname } from 'path';
import { fileURLToPath } from 'url';

const __dirname = fileURLToPath(new URL('.', import.meta.url));

const MIME = {
    '.html': 'text/html',
    '.js': 'application/javascript',
    '.mjs': 'application/javascript',
    '.wasm': 'application/wasm',
    '.json': 'application/json',
};

const server = createServer(async (req, res) => {
    const url = new URL(req.url, 'http://localhost');
    console.log(`[server] ${req.method} ${url.pathname}`);
    let pathname = url.pathname === '/' ? '/test.html' : url.pathname;
    const filePath = join(__dirname, pathname);
    try {
        const data = await readFile(filePath);
        const ext = extname(filePath);
        res.writeHead(200, {
            'Content-Type': MIME[ext] || 'application/octet-stream',
            // Required for SharedArrayBuffer (emscripten pthreads).
            'Cross-Origin-Opener-Policy': 'same-origin',
            'Cross-Origin-Embedder-Policy': 'require-corp',
        });
        res.end(data);
    } catch {
        res.writeHead(404);
        res.end('Not found');
    }
});

await new Promise(resolve => server.listen(0, resolve));
const port = server.address().port;
console.log(`Server on http://localhost:${port}`);

const browser = await chromium.launch();
const page = await browser.newPage();

page.on('console', msg => console.log(`[browser] ${msg.text()}`));
page.on('pageerror', err => console.error(`[browser error] ${err.message}`));

// Capture Web Worker console output too.
page.on('worker', worker => {
    console.log(`[worker created] ${worker.url()}`);
    worker.on('close', () => console.log(`[worker closed] ${worker.url()}`));
});

await page.goto(`http://localhost:${port}/test.html`);

// Wait for test result (max 60s — emscripten module loading can be slow).
const result = await page.waitForFunction(() => window.__TEST_RESULT__, { timeout: 60000 });
const value = await result.jsonValue();

await browser.close();
server.close();

if (value === 'PASS') {
    console.log('\nAll emscripten browser tests passed!');
    process.exit(0);
} else {
    console.error('\nEmscripten browser test failed:', value);
    process.exit(1);
}
