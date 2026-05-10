#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
VENV_DIR="${LUCIVY_VENV:-$SCRIPT_DIR/.venv}"

# ── Create venv if needed ───────────────────────────────────────────
if [ ! -d "$VENV_DIR" ]; then
    echo "=== Creating venv at $VENV_DIR ==="
    python3 -m venv "$VENV_DIR"
    "$VENV_DIR/bin/pip" install -q maturin
fi

# ── Isolate from conda ──────────────────────────────────────────────
unset CONDA_PREFIX 2>/dev/null || true
unset CONDA_DEFAULT_ENV 2>/dev/null || true
export VIRTUAL_ENV="$VENV_DIR"
export PATH="$VENV_DIR/bin:$PATH"

# ── Build ───────────────────────────────────────────────────────────
cd "$SCRIPT_DIR"
echo "=== Building lucivy Python binding (release) ==="
maturin develop --release

echo "=== Installed: $(pip show lucivy 2>/dev/null | grep Version) ==="
echo "=== Python: $(python3 --version) ==="
echo "=== Done. Activate with: source $VENV_DIR/bin/activate ==="
