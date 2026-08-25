#!/bin/bash
# Build the publishable Python artefacts in the official maturin container:
#   - one abi3 wheel for every CPython >= 3.9, tagged manylinux_2_28_x86_64
#     (PyPI refuses a bare linux_x86_64 tag, and 2_28 covers every
#     distribution since 2018 — the 2.0.x release was 2_34, and one
#     interpreter only);
#   - the sdist, so other platforms can build from source.
#
# Output: bindings/python/dist/. Publish with
#   .venv/bin/maturin upload dist/*   (or twine)
#
# The whole workspace is mounted: the binding depends on ../.. by path.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
IMAGE="${MATURIN_IMAGE:-ghcr.io/pyo3/maturin:latest}"
MANYLINUX="${MANYLINUX:-2_28}"

rm -rf "$SCRIPT_DIR/dist"
mkdir -p "$SCRIPT_DIR/dist"

echo "=== abi3 wheel, manylinux_${MANYLINUX}, in $IMAGE ==="
docker run --rm \
    -v "$ROOT:/io" \
    -w /io/bindings/python \
    -e CARGO_HOME=/io/target/.cargo-docker \
    "$IMAGE" \
    build --release --manylinux "$MANYLINUX" --out dist

echo "=== sdist ==="
docker run --rm \
    -v "$ROOT:/io" \
    -w /io/bindings/python \
    "$IMAGE" \
    sdist --out dist

ls -la "$SCRIPT_DIR/dist"
