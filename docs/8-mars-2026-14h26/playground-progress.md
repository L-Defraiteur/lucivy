# Playground & Emscripten Snapshot Progress

## Status: EMSCRIPTEN TESTS PASSING — ready for playground rewrite

## What's done

### PyPI 0.2.2 published
- Bumped version, republished with snapshot support (export_snapshot, import_snapshot, etc.)
- `.env` renamed: `PIPY_API_TOKEN` → `MATURIN_PYPI_TOKEN`, `CRATESIO_API_TOKEN` → `CARGO_REGISTRY_TOKEN`

### Emscripten binding: snapshot + memory + destroy fixes
- **build.sh**: `EXPORTED_RUNTIME_METHODS` now includes `getValue` and `HEAPU8` (needed for safe memory access with pthreads + ALLOW_MEMORY_GROWTH)
- **lucivy-worker.js**:
  - `exportSnapshot`: uses `Module.getValue(lenPtr, 'i32')` + `Module.HEAPU8.slice()` (safe after memory growth)
  - `importSnapshot`: uses `Module.HEAPU8.set()` for copying data into WASM memory
  - `close`/`destroy`: do NOT call `lucivy_destroy` (see below)
  - OPFS sync is best-effort everywhere (try/catch or `.catch(() => {})`)
- **lucivy.js**: `importSnapshot(data, path)` on Lucivy, `exportSnapshot()` on LucivyIndex
- **lucivy.d.ts**: corresponding type declarations

### Emscripten destroy fix — key finding
**Problem**: `lucivy_destroy` → `drop(Box<LucivyContext>)` → drops IndexWriter → joins merge threads.
With emscripten pthreads, the thread join blocks the worker's event loop, deadlocking all subsequent messages.

**Solution**: `close` and `destroy` in the worker only remove the index from the `indexes` Map.
The actual WASM memory is reclaimed when `lucivy.terminate()` kills the worker.
This is safe because:
- Individual `close()`/`destroy()` is instant
- `destroy()` also does best-effort OPFS cleanup
- `terminate()` kills the worker → all WASM memory freed
- No deadlock possible

### Emscripten WASM rebuilt
- `bash build.sh` completed successfully
- Output: `bindings/emscripten/pkg/lucivy.js` + `lucivy.wasm` (~4MB)

### Playwright test passing
- `bindings/emscripten/test.html`: Full test including snapshot export/import
- `bindings/emscripten/test-runner.mjs`: HTTP server with COOP/COEP headers + Playwright runner
- Tests: create, add, commit, search (BM25 + contains), delete, schema, export snapshot (6384 bytes, LUCE magic verified), import snapshot (2 docs restored, search works), destroy (both indexes), terminate

## Emscripten pthreads gotchas (reference)

1. **HEAPU8/HEAPU32 stale with ALLOW_MEMORY_GROWTH**: TypedArray views become undefined after memory grows. Fix: export `HEAPU8` and `getValue` via `EXPORTED_RUNTIME_METHODS`.
2. **Module.wasmMemory not exposed with MODULARIZE=1**: Not on Module object by default. Would need to be in EXPORTED_RUNTIME_METHODS.
3. **lucivy_destroy deadlocks**: IndexWriter drop joins threads via the worker event loop → deadlock. Fix: don't call it, let terminate() handle cleanup.
4. **OPFS can hang**: `navigator.storage.getDirectory()` may never resolve in some contexts (headless, no secure context). Always use best-effort patterns.
5. **Thread pool exhaustion**: PTHREAD_POOL_SIZE=8. Each index uses ~1 writer thread + merge threads. Multiple indexes can exhaust the pool.
6. **COOP/COEP required**: SharedArrayBuffer needs `Cross-Origin-Opener-Policy: same-origin` + `Cross-Origin-Embedder-Policy: require-corp` headers.

## Next steps

1. **Rebuild playground** (`playground/index.html`) to use emscripten API instead of wasm-bindgen:
   - Use `Lucivy` class from `bindings/emscripten/js/lucivy.js` (worker-based)
   - Needs COOP/COEP headers for SharedArrayBuffer (GitHub Pages needs `coi-serviceworker`)
   - `importSnapshot` for demo dataset, `create`+`add` for user files
2. **Republish lucivy-wasm on npm** (the wasm-bindgen package — separate from emscripten)

## Playground architecture

### Files created
- `playground/index.html` — single-file app, dark theme, search UI with tabs (demo/user)
- `playground/build_dataset.py` — generates dataset.luce from source tree (532 files, 8.7MB)
- `playground/dataset.luce` — pre-built snapshot
- `playground/lucivy_wasm.js` + `lucivy_wasm_bg.wasm` — OLD wasm-bindgen files (need to switch to emscripten)

### Design
- Two indexes: "Lucivy source code" (demo, loaded from dataset.luce) + "Your files" (user import)
- User can import .txt, .md, .rs, .py, .js, .zip, .tar.gz
- Tab switching between indexes
- Highlights with byte-offset → char-offset mapping for user files
- For GitHub Pages: needs COOP/COEP headers — use `coi-serviceworker` workaround

## LinkedIn/docs also created
- `docs/7-mars-2026-08h35/linkedin-post-v2.md` (EN + FR)
- `docs/7-mars-2026-08h35/image-prompt.md` (7 Gemini image prompt options)
- `docs/7-mars-2026-08h35/chatgpt-growth-advice.md`

## CI fix committed
- `.github/workflows/ci.yml`: Fixed nodejs test — `lucivy.linux-x64-gnu.node` → `lucivy.node`
- Commit `a38b559`, pushed to main
