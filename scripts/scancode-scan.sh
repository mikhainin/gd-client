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

if [ ! -x "$VENV_DIR/bin/scancode" ]; then
  echo "--- Setting up ScanCode Toolkit in $VENV_DIR ---"
  uv venv "$VENV_DIR" --python 3.12
  uv pip install --python "$VENV_DIR" scancode-toolkit
fi

mkdir -p .scancode

echo "--- Scanning repository source for embedded licenses/copyrights ---"
"$VENV_DIR/bin/scancode" \
  --license --copyright --info \
  --ignore "target/*" --ignore "$VENV_DIR/*" --ignore ".scancode/*" --ignore "Cargo.lock" \
  --json-pp .scancode/scancode-report.json \
  crates/ scripts/ Cargo.toml AGENTS.md README.md "$@"

echo "--- Report written to .scancode/scancode-report.json ---"
