#!/usr/bin/env bash
# Submits this repository's Rust sources to Stanford's MOSS
# (Measure Of Software Similarity) service for plagiarism/similarity
# detection: https://theory.stanford.edu/~aiken/moss/
#
# IMPORTANT CAVEATS (see AGENTS.md change log entry for 2026-09-11):
#   * MOSS's client script has a fixed, hardcoded language list that does
#     NOT include Rust: c, cc, java, ml, pascal, ada, lisp, scheme, haskell,
#     fortran, ascii, vhdl, perl, matlab, python, mips, prolog, spice, vb,
#     csharp, modula2, a8086, javascript, plsql. The closest fallback is
#     "ascii" (plain-text/whitespace tokenization), which loses Rust-aware
#     tokenization and will produce weaker matches than PMD CPD or Dolos.
#   * MOSS requires a personal userid issued by email after registering at
#     https://theory.stanford.edu/~aiken/moss/ - there is no anonymous or
#     shared access. Set MOSS_USERID to your registered id before running.
set -euo pipefail
cd "$(dirname "$0")/.."

TOOLS_DIR=".tools-moss"
MOSS_SCRIPT="$TOOLS_DIR/moss.pl"
MOSS_SCRIPT_URL="http://moss.stanford.edu/general/scripts/mossnet"
MOSS_LANG="${MOSS_LANG:-ascii}"
MOSS_SERVER="moss.stanford.edu"
MOSS_PORT="7690"

echo "--- Validating prerequisites ---"
if ! command -v perl >/dev/null 2>&1; then
  echo "error: perl is required but was not found on PATH" >&2
  exit 1
fi
echo "perl: $(perl -v | grep -o 'v[0-9][0-9.]*' | head -1)"

if [ -z "${MOSS_USERID:-}" ]; then
  cat >&2 <<'EOF'
error: MOSS_USERID is not set.
       MOSS requires a personal userid, issued by email after registering at:
         https://theory.stanford.edu/~aiken/moss/
       Once you have one, re-run as: MOSS_USERID=<your id> scripts/scan-moss.sh
EOF
  exit 1
fi
if ! [[ "$MOSS_USERID" =~ ^[0-9]+$ ]]; then
  echo "error: MOSS_USERID must be numeric, got: $MOSS_USERID" >&2
  exit 1
fi

if [ "$MOSS_LANG" = "rust" ]; then
  echo "error: MOSS has no 'rust' language mode; unset MOSS_LANG or set it to 'ascii' (the closest fallback)." >&2
  exit 1
fi
if [ "$MOSS_LANG" != "ascii" ]; then
  echo "warning: MOSS_LANG=$MOSS_LANG - make sure this is one of MOSS's supported languages (rust is NOT supported)." >&2
fi

echo "--- Checking connectivity to $MOSS_SERVER:$MOSS_PORT ---"
if ! timeout 10 bash -c "cat < /dev/null > /dev/tcp/$MOSS_SERVER/$MOSS_PORT" 2>/dev/null; then
  echo "error: cannot reach $MOSS_SERVER:$MOSS_PORT (network/firewall issue, or the service is down)" >&2
  exit 1
fi
echo "$MOSS_SERVER:$MOSS_PORT is reachable."

mkdir -p "$TOOLS_DIR"
if [ ! -f "$MOSS_SCRIPT" ]; then
  echo "--- Downloading MOSS client script into $TOOLS_DIR (one-time) ---"
  curl -fsSL -o "$MOSS_SCRIPT" "$MOSS_SCRIPT_URL"
fi

if ! grep -q '^\$userid=' "$MOSS_SCRIPT"; then
  echo "error: could not find the \$userid= line in $MOSS_SCRIPT (upstream script format may have changed)" >&2
  exit 1
fi

# Patch a throwaway copy with the caller's registered userid rather than
# mutating the cached script, so re-runs with a different MOSS_USERID don't
# require re-downloading.
RUN_SCRIPT="$TOOLS_DIR/moss-run.pl"
sed "s/^\\\$userid=.*/\$userid=$MOSS_USERID;/" "$MOSS_SCRIPT" > "$RUN_SCRIPT"

mapfile -t RUST_FILES < <(find crates -name '*.rs' -not -path '*/target/*')
if [ "${#RUST_FILES[@]}" -eq 0 ]; then
  echo "error: no .rs files found under crates/" >&2
  exit 1
fi
echo "Found ${#RUST_FILES[@]} Rust source files to scan."

echo "--- Submitting to MOSS (language=$MOSS_LANG) ---"
perl "$RUN_SCRIPT" -l "$MOSS_LANG" "${RUST_FILES[@]}"
