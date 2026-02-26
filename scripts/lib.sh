#!/usr/bin/env bash
# Shared helpers for scripts/.
# Source this file: source "$(dirname "$0")/lib.sh"

# ---------------------------------------------------------------------------
# Required tools
# ---------------------------------------------------------------------------

require_cmd() {
    if ! command -v "$1" &>/dev/null; then
        echo "ERROR: $1 is not installed."
        if [[ -n "${2:-}" ]]; then echo "  $2"; fi
        exit 1
    fi
}

require_cmd cargo "Install Rust toolchain: https://rustup.rs"
require_cmd python3 "sudo apt install python3  OR  brew install python3"

# ---------------------------------------------------------------------------
# Resolve cargo target directory dynamically.
# Works regardless of custom target-dir settings in .cargo/config.toml.
# ---------------------------------------------------------------------------

CARGO_TARGET_DIR=$(cargo metadata --format-version 1 --no-deps 2>/dev/null \
    | python3 -c "import sys,json; print(json.load(sys.stdin)['target_directory'])") || {
    echo "ERROR: Failed to resolve cargo target directory."
    echo "  Ensure you are in a Cargo workspace."
    exit 1
}

PBFHOGG="${CARGO_TARGET_DIR}/release/pbfhogg"

# ---------------------------------------------------------------------------
# Portable file size in MB (works on both GNU and BSD/macOS stat).
# Usage: MB=$(file_size_mb "$path")
# ---------------------------------------------------------------------------

file_size_mb() {
    local bytes
    if bytes=$(stat -Lc%s "$1" 2>/dev/null); then
        :
    else
        bytes=$(stat -f%z "$1" 2>/dev/null) || {
            echo "ERROR: Cannot determine file size of $1"
            exit 1
        }
    fi
    echo $(( bytes / 1000000 ))
}

# ---------------------------------------------------------------------------
# Extract executable path from cargo build --message-format=json output.
# Pipe JSON lines to this function; returns the last executable found.
# Usage: BIN=$(cargo build ... --message-format=json 2>/dev/null | cargo_bin_path)
# ---------------------------------------------------------------------------

cargo_bin_path() {
    python3 -c "
import sys, json
last = None
for line in sys.stdin:
    try:
        msg = json.loads(line)
        if msg.get('executable'):
            last = msg['executable']
    except:
        pass
if last:
    print(last)
"
}

# ---------------------------------------------------------------------------
# Current time in epoch milliseconds (portable, no GNU date +%s%N needed).
# Usage: START=$(epoch_ms)
# ---------------------------------------------------------------------------

epoch_ms() {
    python3 -c "import time; print(int(time.time()*1000))"
}
