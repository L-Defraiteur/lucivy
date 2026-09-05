#!/bin/bash
set -euo pipefail

# ── Configuration ───────────────────────────────────────────────────────────
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$SCRIPT_DIR/../.."
OUT_DIR="$SCRIPT_DIR/pkg"
EMSDK_DIR="${EMSDK_DIR:-$HOME/emsdk}"

# ── Source emsdk ────────────────────────────────────────────────────────────
if [ -f "$EMSDK_DIR/emsdk_env.sh" ]; then
    source "$EMSDK_DIR/emsdk_env.sh" 2>/dev/null
fi

if ! command -v emcc &>/dev/null; then
    echo "ERROR: emcc not found. Set EMSDK_DIR or source emsdk_env.sh" >&2
    exit 1
fi

echo "=== Step 1: Build Rust staticlib for wasm32-unknown-emscripten ==="

export EMCC_CFLAGS="-pthread -fexceptions -sDISABLE_EXCEPTION_CATCHING=0"
export RUSTFLAGS="-C target-feature=+atomics,+bulk-memory,+mutable-globals -C panic=abort"

cd "$ROOT_DIR"
cargo +nightly build \
    -p lucivy-emscripten \
    --target wasm32-unknown-emscripten \
    --release \
    -Z build-std=std,panic_abort

STATIC_LIB="$ROOT_DIR/target/wasm32-unknown-emscripten/release/liblucivy_emscripten.a"
if [ ! -f "$STATIC_LIB" ]; then
    echo "ERROR: static lib not found at $STATIC_LIB" >&2
    exit 1
fi
echo "Static lib: $STATIC_LIB"

echo "=== Step 2: Link with emcc ==="
mkdir -p "$OUT_DIR"

# No ASYNCIFY: with the WASMFS OPFS backend on pthreads, every OPFS call in
# the proxy worker went through Asyncify while checkMailbox re-entered the
# next call inside the unwound stack (nested handleSleep -> `unreachable`,
# seen as a hang of the first segment write). Nothing here sleeps: blocking
# waits are futexes on pthreads, the commit runs on its own thread and the
# worker polls its status through the SharedArrayBuffer.
# The pthread pool is sized at startup, not at build time: emscripten accepts a
# JavaScript expression here, so one binary adapts to the machine instead of
# needing one build per core count. It was a fixed 8 while the luciole
# scheduler asked for `available_parallelism()` — 24 on the development
# machine — and the observed concurrency of a query was 4, the pool minus what
# the actors hold. Capped at 16: each thread costs a 2 MB stack against a 4 GB
# address space, and beyond that the queries stop being the bottleneck.
#
# Careful: more threads did not help queries either (measured 25 August:
# 4 -> 12 scheduler threads made the panel 7 % slower — the engine waits on
# memory in WASM, not on CPU), and it is actively harmful for indexing —
# merge parallelism is what exhausted the address space on 24 August
# (LUCIVY_MERGE_CONCURRENCY is 1 on wasm) and segment size is what did on
# 25 August. Raising this raises none of those; the scheduler is sized by
# LUCIVY_SCHEDULER_THREADS (default 4, measured).
emcc "$STATIC_LIB" \
    -o "$OUT_DIR/lucivy.js" \
    -pthread \
    -sPTHREAD_POOL_SIZE='Math.min(navigator.hardwareConcurrency || 4, 16)' \
    -sPTHREAD_POOL_SIZE_STRICT=0 \
    -sALLOW_MEMORY_GROWTH=1 \
    -sMAXIMUM_MEMORY=4GB \
    -sMODULARIZE=1 \
    -sEXPORT_NAME=createLucivy \
    -sSTACK_SIZE=2MB \
    -sWASMFS \
    -sEXPORTED_FUNCTIONS='[
        "_lucivy_configure",
        "_lucivy_create",
        "_lucivy_open",
        "_lucivy_open_begin",
        "_lucivy_import_file",
        "_lucivy_open_finish",
        "_lucivy_close",
        "_lucivy_destroy",
        "_lucivy_add",
        "_lucivy_add_many",
        "_lucivy_remove",
        "_lucivy_drop_index",
        "_lucivy_update",
        "_lucivy_commit",
        "_lucivy_commit_async",
        "_lucivy_commit_status_ptr",
        "_lucivy_commit_finish",
        "_lucivy_compact_async",
        "_lucivy_drain_merges",
        "_lucivy_dump_mermaid",
        "_lucivy_dump_state",
        "_lucivy_test_condvar",
        "_lucivy_test_coop",
        "_lucivy_test_fs_task",
        "_lucivy_dump_wait_graph",
        "_lucivy_dump_wait_graph_text",
        "_lucivy_search",
        "_lucivy_search_filtered",
        "_lucivy_query_warnings",
        "_lucivy_memory_status",
        "_lucivy_preload",
        "_lucivy_export_snapshot",
        "_lucivy_import_snapshot",
        "_lucivy_shard_versions",
        "_lucivy_export_sharded_delta",
        "_lucivy_apply_sharded_delta",
        "_lucivy_merge_stats",
        "_lucivy_export_stats",
        "_lucivy_search_with_global_stats",
        "_lucivy_num_docs",
        "_lucivy_schema_json",
        "_lucivy_read_logs",
        "_lucivy_log_ring_ptr",
        "_lucivy_log_ring_size",
        "_malloc",
        "_free",
        "_main"
    ]' \
    -sEXPORTED_RUNTIME_METHODS='["ccall","cwrap","UTF8ToString","stringToUTF8","lengthBytesUTF8","getValue","HEAPU8"]' \
    -sWASM_BIGINT \
    -sEXPORT_ES6=1 \
    -sPROXY_TO_PTHREAD \
    -fexceptions \
    -sDISABLE_EXCEPTION_CATCHING=0 \
    -O2 \
    -sMALLOC=${LUCIVY_WASM_MALLOC:-mimalloc} \
    ${LUCIVY_WASM_DEBUG:+-g2 -sASSERTIONS=1 -sSTACK_OVERFLOW_CHECK=2}
# LUCIVY_WASM_MALLOC=mimalloc|dlmalloc|emmalloc: the allocator. Emscripten's
# default, dlmalloc, takes one global lock under pthreads; the query paths that
# cross token boundaries (relaxed contains, fuzzy, boolean parse) allocate a
# lot, and four threads serialised on that lock. Measured 25 August, 10 000
# kernel files, 21-query panel, same index and same page: dlmalloc 551 ms per
# query (median 244), mimalloc 188 (median 107) — relaxed `kmalloc` 429 -> 106,
# fuzzy d1 1 057 -> 184, boolean parse 498 -> 59. The ratio to native went from
# 2x-20x depending on the query to a flat 2-3x. mimalloc is the default.
# LUCIVY_WASM_DEBUG=1: keep function names (symbolised traps in the browser
# console), runtime assertions and stack-overflow cookies. Bigger and slower —
# for diagnosing a "memory access out of bounds" in a pthread, not for shipping.

echo "=== Step 3: Copy to playground ==="
PLAYGROUND_PKG="$ROOT_DIR/playground/pkg"
mkdir -p "$PLAYGROUND_PKG"
cp "$OUT_DIR"/lucivy.* "$PLAYGROUND_PKG"/
echo "Copied to $PLAYGROUND_PKG"

# The JS layer too, not only the wasm. `playground/js/` is a copy of
# `bindings/emscripten/js/`, and it was kept in sync by hand: on 28 August the
# two silently diverged for an afternoon, so the playground exercised an older
# binding than the one about to be published — which is exactly the case the
# playground exists to catch. Copying it here removes the possibility.
PLAYGROUND_JS="$ROOT_DIR/playground/js"
mkdir -p "$PLAYGROUND_JS"
cp "$SCRIPT_DIR/js/lucivy.js" "$SCRIPT_DIR/js/lucivy-worker.js" "$PLAYGROUND_JS"/
echo "Copied the JS layer to $PLAYGROUND_JS"

# ── The page's version, from Cargo.toml ─────────────────────────────────────
# Two literals in index.html used to be edited by hand: the version in the
# terminal banner, and BUILD, which is the cache-buster on the worker URL. Both
# sat at 3.0.4 through three releases — the banner lied to every visitor, and
# the stale cache-buster meant a returning one kept the worker and the wasm
# from before the fixes. Writing them here makes Cargo.toml the only place a
# version is decided, and makes the buster move whenever the engine does.
VERSION=$(grep -m1 '^version' "$ROOT_DIR/Cargo.toml" | cut -d'"' -f2)
GITSHA=$(git -C "$ROOT_DIR" rev-parse --short HEAD 2>/dev/null || echo nogit)
INDEX="$ROOT_DIR/playground/index.html"
if [ -f "$INDEX" ]; then
    # The banner: ░▒▓█  <version>  █▓▒░
    sed -i -E "s/(░▒▓█  )[0-9]+\.[0-9]+\.[0-9]+(  █▓▒░)/\1${VERSION}\2/" "$INDEX"
    # The cache-buster: version plus the commit, so two builds of one version
    # are still distinguishable to a browser.
    sed -i -E "s/(const BUILD = ')[^']*(')/\1${VERSION}-${GITSHA}\2/" "$INDEX"
    echo "index.html stamped: version ${VERSION}, build ${VERSION}-${GITSHA}"
    grep -nE "const BUILD|░▒▓█  [0-9]" "$INDEX" | sed 's/^/  /'
fi

echo "=== Done ==="
echo "Output: $OUT_DIR/lucivy.js + $OUT_DIR/lucivy.wasm"
ls -lh "$OUT_DIR"/lucivy.*
ls -lh "$PLAYGROUND_PKG"/lucivy.*
