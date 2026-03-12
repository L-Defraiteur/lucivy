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

emcc "$STATIC_LIB" \
    -o "$OUT_DIR/lucivy.js" \
    -pthread \
    -sPTHREAD_POOL_SIZE=8 \
    -sPTHREAD_POOL_SIZE_STRICT=0 \
    -sALLOW_MEMORY_GROWTH=1 \
    -sMAXIMUM_MEMORY=1GB \
    -sMODULARIZE=1 \
    -sEXPORT_NAME=createLucivy \
    -sSTACK_SIZE=2MB \
    -sEXPORTED_FUNCTIONS='[
        "_lucivy_create",
        "_lucivy_open_begin",
        "_lucivy_import_file",
        "_lucivy_open_finish",
        "_lucivy_destroy",
        "_lucivy_add",
        "_lucivy_add_many",
        "_lucivy_remove",
        "_lucivy_update",
        "_lucivy_commit",
        "_lucivy_commit_poll",
        "_lucivy_rollback",
        "_lucivy_export_dirty",
        "_lucivy_export_all",
        "_lucivy_search",
        "_lucivy_search_filtered",
        "_lucivy_export_snapshot",
        "_lucivy_import_snapshot",
        "_lucivy_num_docs",
        "_lucivy_schema_json",
        "_malloc",
        "_free",
        "_main"
    ]' \
    -sEXPORTED_RUNTIME_METHODS='["ccall","cwrap","UTF8ToString","stringToUTF8","lengthBytesUTF8","getValue","HEAPU8"]' \
    -sWASM_BIGINT \
    -sEXPORT_ES6=1 \
    -sPROXY_TO_PTHREAD \
    -sASYNCIFY \
    -sASYNCIFY_STACK_SIZE=65536 \
    -fexceptions \
    -sDISABLE_EXCEPTION_CATCHING=0 \
    -O2

echo "=== Done ==="
echo "Output: $OUT_DIR/lucivy.js + $OUT_DIR/lucivy.wasm"
ls -lh "$OUT_DIR"/lucivy.*
