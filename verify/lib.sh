#!/usr/bin/env bash
# Shared helpers for verify scripts.
# Source this file: source "$(dirname "$0")/lib.sh"
#
# Requires $PBFHOGG to be set before sourcing.

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
