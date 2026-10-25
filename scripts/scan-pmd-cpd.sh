#!/usr/bin/env bash
# Runs PMD's Copy-Paste Detector (CPD) against this repository's Rust
# sources. PMD CPD natively understands Rust (--language rust), needs only a
# JVM, and requires no account/registration, so it's the lowest-friction of
# the three similarity/plagiarism scanners evaluated for this repo (see
# AGENTS.md change log entry for 2026-09-11).
#
# Downloads PMD into a throwaway cache dir (gitignored) on first run.
set -euo pipefail
cd "$(dirname "$0")/.."

PMD_VERSION="7.27.0"
TOOLS_DIR=".tools-pmd"
PMD_HOME="$TOOLS_DIR/pmd-bin-$PMD_VERSION"
PMD_BIN="$PMD_HOME/bin/pmd"
MIN_TOKENS="${MIN_TOKENS:-50}"
REPORT_DIR=".cpd-report"
REPORT_FILE="$REPORT_DIR/cpd-report.txt"

echo "--- Validating prerequisites ---"
if ! command -v java >/dev/null 2>&1; then
  echo "error: java is required but was not found on PATH (PMD 7 needs a JVM, Java 11+ recommended)" >&2
  exit 1
fi

JAVA_VERSION_STR="$(java -version 2>&1 | head -1)"
JAVA_MAJOR="$(java -version 2>&1 | head -1 | grep -oE '"[0-9]+' | tr -d '"' | head -1)"
if [ -n "$JAVA_MAJOR" ] && [ "$JAVA_MAJOR" -lt 11 ]; then
  echo "error: PMD 7 requires Java 11+, found: $JAVA_VERSION_STR" >&2
  exit 1
fi
echo "java: $JAVA_VERSION_STR"

if ! command -v unzip >/dev/null 2>&1; then
  echo "error: unzip is required to extract the PMD distribution but was not found on PATH" >&2
  exit 1
fi

if [ ! -x "$PMD_BIN" ]; then
  echo "--- Downloading PMD $PMD_VERSION into $TOOLS_DIR (one-time) ---"
  mkdir -p "$TOOLS_DIR"
  ZIP_PATH="$TOOLS_DIR/pmd-dist-$PMD_VERSION-bin.zip"
  curl -fsSL -o "$ZIP_PATH" \
    "https://github.com/pmd/pmd/releases/download/pmd_releases/$PMD_VERSION/pmd-dist-$PMD_VERSION-bin.zip"
  unzip -q -o "$ZIP_PATH" -d "$TOOLS_DIR"
  rm -f "$ZIP_PATH"
fi

if [ ! -x "$PMD_BIN" ]; then
  echo "error: PMD binary not found/executable at $PMD_BIN after download" >&2
  exit 1
fi

mapfile -t RUST_FILES < <(find crates -name '*.rs' -not -path '*/target/*')
if [ "${#RUST_FILES[@]}" -eq 0 ]; then
  echo "error: no .rs files found under crates/" >&2
  exit 1
fi
echo "Found ${#RUST_FILES[@]} Rust source files to scan."

mkdir -p "$REPORT_DIR"

echo "--- Running PMD CPD (language=rust, minimum-tokens=$MIN_TOKENS) ---"
set +e
"$PMD_BIN" cpd \
  --dir crates \
  --language rust \
  --minimum-tokens "$MIN_TOKENS" \
  --skip-duplicate-files \
  --no-fail-on-violation \
  -r "$REPORT_FILE"
CPD_STATUS=$?
set -e

# Exit codes: 0 = no duplicates/no errors, 4 = duplicates found (suppressed
# above via --no-fail-on-violation), 5 = a recoverable error occurred parsing
# some file(s). Anything else is unexpected.
if [ "$CPD_STATUS" -ne 0 ] && [ "$CPD_STATUS" -ne 4 ]; then
  echo "error: pmd cpd exited with unexpected status $CPD_STATUS" >&2
  exit "$CPD_STATUS"
fi

if [ -s "$REPORT_FILE" ]; then
  echo "--- Duplicate code found; report written to $REPORT_FILE ---"
  cat "$REPORT_FILE"
else
  echo "--- No duplicate code found (threshold: $MIN_TOKENS tokens) ---"
fi
