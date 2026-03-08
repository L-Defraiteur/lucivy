# Session 3 — Stored fields in search results + playground improvements

## Completed

### `fields` option added to all bindings

New `fields` (or `include_fields`) parameter on search — returns stored field values (path, content, title, etc.) alongside docId/score/highlights.

**Rust (emscripten binding)**:
- `bindings/emscripten/src/lib.rs`: Added `include_fields: i32` param to `lucivy_search` and `lucivy_search_filtered` FFI. `collect_results` now accepts `include_fields: bool`, iterates `doc.field_values()`, skips internal fields (`_node_id`, `*_raw`, `*_ngram`), extracts str/u64/i64/f64 as `serde_json::Value`.
- Import fix: `use ld_lucivy::schema::Value;` needed for `.as_value()` trait method.
- `SearchResultJson` now has `fields: Option<HashMap<String, serde_json::Value>>`.
- Rebuild emscripten OK (build.sh, same EXPORTED_FUNCTIONS — no new FFI symbol, just new param on existing ones).

**Rust (Python binding)**:
- `bindings/python/src/lib.rs`: `SearchResult` gets `fields: Option<HashMap<String, String>>`. `search()` signature: `#[pyo3(signature = (query, limit=10, highlights=false, allowed_ids=None, fields=false))]`. `collect_results` gets `include_fields: bool`.
- Tested: `idx.search('rust', fields=True)` → `r.fields['title']` works.

**Rust (Node.js binding)**:
- `bindings/nodejs/src/lib.rs`: `SearchResult` gets `fields: Option<HashMap<String, String>>`. `SearchOptions` gets `fields: Option<bool>`. Same `collect_results` pattern.
- Tested: `idx.search('rust', { fields: true })` → `r.fields.title` works.

**JS (emscripten worker + lucivy.js)**:
- `js/lucivy-worker.js`: passes `args.fields ? 1 : 0` as new param to `callStr('lucivy_search', ...)` and `Module.ccall('lucivy_search_filtered', ...)`.
- `js/lucivy.js`: `search()` and `searchFiltered()` pass `fields: options.fields` to worker.
- `js/lucivy.d.ts`: `SearchOptions.fields?: boolean`, `SearchResult.fields?: Record<string, string | number>`.

### Playground improvements

- **File paths shown**: Results now display `r.fields.path` (e.g. `src/core/searcher.rs`) instead of `doc #333`.
- **Snippets with highlights**: `buildSnippets()` function — converts byte offsets to char offsets, merges nearby highlights into context windows (~120 chars), renders `<mark>` tags. Up to 5 snippet windows per result.
- **Without highlights**: Shows first 300 chars of content as preview.
- **Results open by default**: No need to click to expand.
- **Removed `applyHighlights()`**: Replaced by `buildSnippets()`.
- **Snapshot export/import buttons**: In the "Import your own files" panel. Export downloads `.luce` file, import loads a `.luce` as user index.
- **CSS**: `.snippets` pre style, `.snapshot-actions` button styles.
- **`serve.mjs`**: Simple dev server with COOP/COEP headers for local testing.
- **`test-playground.mjs`**: Updated to verify file paths and highlight marks.

### Tests

- **Rust**: New `test_stored_fields_retrieval` in `lucivy_core/src/handle.rs` — creates index, adds doc with path+content, searches, retrieves stored fields via `doc.field_values()`, asserts field values. **57/57 passed**.
- **ld-lucivy**: 1066/1066 passed (no regressions).
- **Node.js**: `test.mjs` — added tests #10 (fields) and #11 (highlights+fields). **All passed**.
- **Python**: Inline test — `fields=True`, `fields+highlights`, `fields=False` default. **All passed**.
- **Playground**: Playwright — startup, search, file path check (`src/core/searcher.rs` not `doc #`), 324 highlight marks. **All passed**.

### CI updated

- `.github/workflows/ci.yml`: Python test section now includes `fields=True` assertions.
- Node.js test: `bindings/nodejs/test.mjs` tests fields (runs via `node bindings/nodejs/test.mjs` in CI).

### READMEs updated

- `README.md` (main): New "Fields (stored values)" section before Snapshots.
- `bindings/python/README.md`: `fields=True` example in Search section.
- `bindings/nodejs/README.md`: `{ fields: true }` example in Search section.
- `bindings/emscripten/README.md`: `{ fields: true }` example + v0.4.0 note.

### npm version bump

- `bindings/emscripten/package.json`: `0.3.1` → `0.4.0`.
- **Not yet published**. Waiting for commit + push.

### Utilities

- `playground/rag3db-docs.zip`: 238 markdown files from `rag3db/docs/` (991K) for testing custom file import.

## Pending

1. **Commit + push** all changes
2. **Publish npm** `lucivy-wasm@0.4.0`
3. **Publish crates.io** if needed (the `fields` change is in bindings, not in `ld-lucivy` or `lucivy-core` core crates — only the test was added to lucivy-core)
4. **coi-serviceworker** for GitHub Pages deployment
5. **Rebuild `dataset.luce`** if we want to update it (current one works fine, 532 files)

## Key architecture note

The `fields` feature uses tantivy's stored fields (`doc.field_values()`). Fields are stored by default in lucivy (`stored.unwrap_or(true)` in `build_schema`). The `collect_results` function in each binding already calls `searcher.doc(doc_addr)` to get `_node_id` — adding field extraction is zero extra I/O (document already fetched). Internal fields (`_node_id`, `*_raw`, `*_ngram`) are filtered out.
