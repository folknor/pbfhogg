#!/usr/bin/env bash
# Shared helpers for verify scripts.
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
require_cmd osmium "sudo apt install osmium-tool  OR  brew install osmium-tool"

# ---------------------------------------------------------------------------
# Resolve PBFHOGG binary path dynamically from cargo metadata.
# Works regardless of custom target-dir settings in .cargo/config.toml.
# ---------------------------------------------------------------------------

CARGO_TARGET_DIR=$(cargo metadata --format-version 1 --no-deps 2>/dev/null \
    | python3 -c "import sys,json; print(json.load(sys.stdin)['target_directory'])")
PBFHOGG="${CARGO_TARGET_DIR}/release/pbfhogg"

# Assert that a PBF file's header contains Sort.Type_then_ID.
# Usage: assert_sorted FILE LABEL
assert_sorted() {
    local file="$1"
    local label="$2"
    local info
    info=$("$PBFHOGG" fileinfo "$file")
    if [[ "$info" == *"Sort.Type_then_ID"* ]]; then
        echo "  PASS: $label has Sort.Type_then_ID"
    else
        echo "  FAIL: $label missing Sort.Type_then_ID"
        exit 1
    fi
}

# Compare Sort.Type_then_ID feature between pbfhogg and osmium PBF output.
# Fails if osmium has it but pbfhogg doesn't.
# Usage: compare_sort_feature PBFHOGG_FILE OSMIUM_FILE
compare_sort_feature() {
    local pf="$1"
    local of="$2"
    local p_info o_info p_sorted o_sorted

    p_info=$("$PBFHOGG" fileinfo "$pf")
    o_info=$("$PBFHOGG" fileinfo "$of")

    p_sorted=no
    o_sorted=no
    if [[ "$p_info" == *"Sort.Type_then_ID"* ]]; then p_sorted=yes; fi
    if [[ "$o_info" == *"Sort.Type_then_ID"* ]]; then o_sorted=yes; fi

    echo "  pbfhogg Sort.Type_then_ID: $p_sorted"
    echo "  osmium  Sort.Type_then_ID: $o_sorted"

    if [[ "$o_sorted" == "yes" ]]; then
        if [[ "$p_sorted" == "no" ]]; then
            echo "  FAIL: osmium has Sort.Type_then_ID but pbfhogg does not"
            exit 1
        fi
    fi
}
