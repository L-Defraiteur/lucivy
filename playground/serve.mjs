#!/usr/bin/env node
// Serve playground locally with COOP/COEP headers (required for SharedArrayBuffer).
// This replaces coi-serviceworker — no more stale WASM caching issues.
//
// Usage: node playground/serve.mjs [port]
import http from 'node:http';
import fs from 'node:fs';
import path from 'node:path';

const ROOT = path.dirname(new URL(import.meta.url).pathname);
const PORT = parseInt(process.argv[2] || '9877', 10);

const MIME = {
  '.html': 'text/html',
  '.js': 'application/javascript',
  '.mjs': 'application/javascript',
  '.wasm': 'application/wasm',
  '.luce': 'application/octet-stream',
  '.css': 'text/css',
  '.json': 'application/json',
  '.zip': 'application/zip',
};

const server = http.createServer((req, res) => {
  const url = new URL(req.url, `http://localhost:${PORT}`);
  let reqPath = decodeURIComponent(url.pathname);
  if (reqPath === '/') reqPath = '/index.html';

  const filePath = path.join(ROOT, reqPath);

  // Prevent directory traversal
  if (!filePath.startsWith(ROOT)) { res.writeHead(403); res.end('Forbidden'); return; }
  if (!fs.existsSync(filePath) || fs.statSync(filePath).isDirectory()) {
    res.writeHead(404); res.end('Not found'); return;
  }

  const ext = path.extname(filePath);
  res.writeHead(200, {
    'Content-Type': MIME[ext] || 'application/octet-stream',
    // Required for SharedArrayBuffer (pthreads)
    'Cross-Origin-Opener-Policy': 'same-origin',
    'Cross-Origin-Embedder-Policy': 'require-corp',
    'Cross-Origin-Resource-Policy': 'same-origin',
    // Never cache — we need fresh WASM after every rebuild
    'Cache-Control': 'no-store, no-cache, must-revalidate',
  });
  fs.createReadStream(filePath).pipe(res);
});

server.listen(PORT, () => {
  console.log(`\n  Playground: http://localhost:${PORT}`);
  console.log('  COOP/COEP headers active — SharedArrayBuffer enabled');
  console.log('  Cache-Control: no-store — always fresh files');
  console.log('  Ctrl+C to stop\n');
});
