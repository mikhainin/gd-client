#!/usr/bin/env bash
# Scans this repository's own source (not third-party dependencies - see
# `deny.toml`/cargo-deny for that) for embedded license text or copyright
# notices using ScanCode Toolkit, run inside a throwaway uv-managed venv so
# it doesn't need system-wide Python packages.
#
# Requires `uv` (https://docs.astral.sh/uv/). On Debian/Ubuntu without uv:
#   curl -LsSf https://astral.sh/uv/install.sh | sh
set -euo pipefail
cd "$(dirname "$0")/.."

if ! command -v uv >/dev/null 2>&1; then
  echo "error: uv is required (https://docs.astral.sh/uv/) but was not found on PATH" >&2
  exit 1
fi

VENV_DIR=".venv-scancode"
export UV_HTTP_TIMEOUT=30

if [ ! -x "$VENV_DIR/bin/python" ]; then
  echo "--- Setting up ScanCode Toolkit in $VENV_DIR ---"
  uv venv "$VENV_DIR" --python 3.12
fi

echo "--- Ensuring compatible ScanCode dependencies ---"
uv pip install --python "$VENV_DIR/bin/python" \
  --requirement scripts/scancode-requirements.txt

mkdir -p .scancode

echo "--- Scanning repository source for embedded licenses/copyrights ---"
ROOT_NAME=$(basename "$PWD")
"$VENV_DIR/bin/scancode" \
  --license --copyright --info \
  --ignore "*" \
  --include "$ROOT_NAME/crates/*" --include "$ROOT_NAME/scripts/*" \
  --include "$ROOT_NAME/Cargo.toml" --include "$ROOT_NAME/AGENTS.md" \
  --include "$ROOT_NAME/README.md" \
  --json-pp .scancode/scancode-report.json \
  . "$@"

echo "--- Report written to .scancode/scancode-report.json ---"
