#!/usr/bin/env bash
# Runs Dolos (https://dolos.ugent.be/) against this repository's Rust
# sources to detect similar/duplicated code via its tree-sitter-based Rust
# tokenizer.
#
# Requires Node.js >= 24 on PATH. Older Node versions (tested: 18, 20, 22)
# fail to build @dodona/dolos-parsers' native tree-sitter addon: node-gyp
# pulls in an undici version that calls `webidl.util.markAsUncloneable`,
# which those Node releases don't provide, so `npm install` aborts with a
# node-gyp/undici error (or, if scripts are skipped, `dolos` segfaults at
# runtime because the native parser was never built). This is a known
# upstream incompatibility, not something this script can work around - see
# AGENTS.md change log entry for 2026-09-11.
set -euo pipefail
cd "$(dirname "$0")/.."

TOOLS_DIR=".tools-dolos"
DOLOS_VERSION="^2.9.3"
REPORT_DIR=".dolos-report"
MIN_NODE_MAJOR=24

echo "--- Validating prerequisites ---"
if ! command -v node >/dev/null 2>&1; then
  echo "error: node (>= $MIN_NODE_MAJOR) is required but was not found on PATH" >&2
  echo "       Install e.g. via https://nodejs.org/dist/ or nvm; no sudo needed for a user-local install." >&2
  exit 1
fi

NODE_VERSION="$(node -v)"
NODE_MAJOR="${NODE_VERSION#v}"
NODE_MAJOR="${NODE_MAJOR%%.*}"
if [ "$NODE_MAJOR" -lt "$MIN_NODE_MAJOR" ]; then
  echo "error: node $NODE_VERSION found, but Dolos' native Rust parser needs Node >= $MIN_NODE_MAJOR to build (see script header for why)." >&2
  exit 1
fi
echo "node: $NODE_VERSION"

if ! command -v npm >/dev/null 2>&1; then
  echo "error: npm is required but was not found on PATH" >&2
  exit 1
fi

mkdir -p "$TOOLS_DIR"
if [ ! -f "$TOOLS_DIR/package.json" ]; then
  cat > "$TOOLS_DIR/package.json" <<EOF
{
  "private": true,
  "dependencies": {
    "@dodona/dolos": "$DOLOS_VERSION"
  }
}
EOF
fi

if [ ! -x "$TOOLS_DIR/node_modules/.bin/dolos" ]; then
  echo "--- Installing Dolos into $TOOLS_DIR (one-time, builds a native tree-sitter addon) ---"
  (cd "$TOOLS_DIR" && npm install)
fi

if [ ! -x "$TOOLS_DIR/node_modules/.bin/dolos" ]; then
  echo "error: dolos CLI not found/executable at $TOOLS_DIR/node_modules/.bin/dolos after install" >&2
  exit 1
fi

mapfile -t RUST_FILES < <(find crates -name '*.rs' -not -path '*/target/*')
if [ "${#RUST_FILES[@]}" -eq 0 ]; then
  echo "error: no .rs files found under crates/" >&2
  exit 1
fi
echo "Found ${#RUST_FILES[@]} Rust source files to scan."

rm -rf "$REPORT_DIR"

echo "--- Running Dolos (language=rust, output-format=csv) ---"
# csv output is used instead of the default 'terminal'/'web' formats: terminal
# output is a fixed-width table that's hard to read for many files, and 'web'
# starts an interactive server that never exits on its own.
"$TOOLS_DIR/node_modules/.bin/dolos" run \
  --language rust \
  --output-format csv \
  --output-destination "$REPORT_DIR" \
  "${RUST_FILES[@]}"

PAIRS_CSV="$REPORT_DIR/pairs.csv"
echo "--- Report written to $REPORT_DIR/ (metadata.csv, files.csv, kgrams.csv, pairs.csv) ---"
if [ -f "$PAIRS_CSV" ]; then
  TOP_N="${TOP_N:-10}"
  echo "--- Top $TOP_N most similar file pairs ---"
  {
    head -1 "$PAIRS_CSV"
    tail -n +2 "$PAIRS_CSV" | sort -t, -k6 -g -r | head -n "$TOP_N"
  } | column -s, -t
fi
