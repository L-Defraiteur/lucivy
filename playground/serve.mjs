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

// ── Diag state ─────────────────────────────────────────────────────────
const LOG_PATH = path.join(ROOT, 'diag.log');
fs.writeFileSync(LOG_PATH, ''); // clear on start

let evalQueue = [];      // worker eval: [{id, js}]
let evalResults = {};    // id -> {result, error}
let evalMainQueue = [];  // main thread eval: [{id, js}]
let evalMainResults = {};
let evalId = 0;

function readBody(req) {
  return new Promise((resolve) => {
    const chunks = [];
    req.on('data', c => chunks.push(c));
    req.on('end', () => resolve(Buffer.concat(chunks).toString('utf8')));
  });
}

const CORS = {
  'Access-Control-Allow-Origin': '*',
  'Access-Control-Allow-Methods': 'POST, GET, OPTIONS',
  'Access-Control-Allow-Headers': 'Content-Type',
};

const server = http.createServer(async (req, res) => {
  const url = new URL(req.url, `http://localhost:${PORT}`);
  let reqPath = decodeURIComponent(url.pathname);

  // CORS preflight
  if (req.method === 'OPTIONS') {
    res.writeHead(204, CORS);
    res.end();
    return;
  }

  // ── POST /log — append lines to diag.log ──
  if (reqPath === '/log' && req.method === 'POST') {
    const body = await readBody(req);
    const ts = new Date().toTimeString().slice(0, 8);
    const lines = body.split('\n').filter(l => l.length > 0);
    const formatted = lines.map(l => `[${ts}] ${l}\n`).join('');
    fs.appendFileSync(LOG_PATH, formatted);
    res.writeHead(200, { 'Content-Type': 'application/json', ...CORS });
    res.end(JSON.stringify({ ok: true, count: lines.length }));
    return;
  }

  // ── Eval helper ──
  async function handleEval(queue, results, body) {
    let js;
    try { js = JSON.parse(body).js; } catch { js = body; }
    evalId++;
    const id = String(evalId);
    queue.push({ id, js });
    results[id] = null;
    const start = Date.now();
    const poll = () => new Promise((resolve) => {
      const check = setInterval(() => {
        if (results[id] !== null || Date.now() - start > 30000) {
          clearInterval(check);
          const r = results[id] || { error: 'timeout' };
          delete results[id];
          resolve(r);
        }
      }, 100);
    });
    return await poll();
  }

  // ── POST /eval — queue JS for worker execution ──
  if (reqPath === '/eval' && req.method === 'POST') {
    const body = await readBody(req);
    const result = await handleEval(evalQueue, evalResults, body);
    res.writeHead(200, { 'Content-Type': 'application/json', ...CORS });
    res.end(JSON.stringify(result));
    return;
  }

  // ── GET /eval/poll — worker polls for pending commands ──
  if (reqPath === '/eval/poll' && req.method === 'GET') {
    const cmd = evalQueue.shift() || { id: null, js: null };
    res.writeHead(200, { 'Content-Type': 'application/json', ...CORS });
    res.end(JSON.stringify(cmd));
    return;
  }

  // ── POST /eval/result — worker returns eval result ──
  if (reqPath === '/eval/result' && req.method === 'POST') {
    const body = await readBody(req);
    const data = JSON.parse(body);
    if (data.id && data.id in evalResults) {
      evalResults[data.id] = { result: data.result, error: data.error || null };
    }
    res.writeHead(200, { 'Content-Type': 'application/json', ...CORS });
    res.end(JSON.stringify({ ok: true }));
    return;
  }

  // ── POST /eval/main — queue JS for main thread execution ──
  if (reqPath === '/eval/main' && req.method === 'POST') {
    const body = await readBody(req);
    const result = await handleEval(evalMainQueue, evalMainResults, body);
    res.writeHead(200, { 'Content-Type': 'application/json', ...CORS });
    res.end(JSON.stringify(result));
    return;
  }

  // ── GET /eval/main/poll — main thread polls ──
  if (reqPath === '/eval/main/poll' && req.method === 'GET') {
    const cmd = evalMainQueue.shift() || { id: null, js: null };
    res.writeHead(200, { 'Content-Type': 'application/json', ...CORS });
    res.end(JSON.stringify(cmd));
    return;
  }

  // ── POST /eval/main/result — main thread returns result ──
  if (reqPath === '/eval/main/result' && req.method === 'POST') {
    const body = await readBody(req);
    const data = JSON.parse(body);
    if (data.id && data.id in evalMainResults) {
      evalMainResults[data.id] = { result: data.result, error: data.error || null };
    }
    res.writeHead(200, { 'Content-Type': 'application/json', ...CORS });
    res.end(JSON.stringify({ ok: true }));
    return;
  }

  // ── Static files ──
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
  console.log(`  Diag log: ${LOG_PATH}`);
  console.log('  tail -f playground/diag.log');
  console.log('  curl -s localhost:9877/eval -d \'{"js":"1+1"}\'');
  console.log('  Ctrl+C to stop\n');
});
