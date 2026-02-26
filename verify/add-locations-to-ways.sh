#!/usr/bin/env bash
# Cross-validate add-locations-to-ways: pbfhogg vs osmium
# Usage: verify/add-locations-to-ways.sh [input.osm.pbf]
set -euo pipefail
cd "$(dirname "$0")/.."

source "$(dirname "$0")/lib.sh"

INPUT="${1:-data/denmark-latest.osm.pbf}"
OUTDIR="$CARGO_TARGET_DIR/verify/add-locations-to-ways"
mkdir -p "$OUTDIR"

echo "=== Cross-validation add-locations-to-ways ==="
echo "Input: $INPUT"
echo ""

# Build pbfhogg first
scripts/build.sh

echo "--- pbfhogg add-locations-to-ways ---"
time "$PBFHOGG" add-locations-to-ways "$INPUT" -o "$OUTDIR/pbfhogg.osm.pbf"
echo ""

echo "--- osmium add-locations-to-ways ---"
time osmium add-locations-to-ways "$INPUT" -o "$OUTDIR/osmium.osm.pbf" --overwrite
echo ""

echo "=== Element counts ==="
for tool in pbfhogg osmium; do
    f="$OUTDIR/$tool.osm.pbf"
    if [ -f "$f" ]; then
        echo "--- $tool ---"
        "$PBFHOGG" fileinfo --extended "$f"
        echo ""
    else
        echo "--- $tool --- MISSING"
    fi
done

echo "=== Diff (suppress-common) ==="
"$PBFHOGG" diff --suppress-common "$OUTDIR/pbfhogg.osm.pbf" "$OUTDIR/osmium.osm.pbf"
echo ""

echo "=== Sort.Type_then_ID check ==="
compare_sort_feature "$OUTDIR/pbfhogg.osm.pbf" "$OUTDIR/osmium.osm.pbf"
echo ""

# --- Dense index (verify output matches hash) ---
# Dense index requires 128 GB virtual memory (vm.overcommit_memory=1 or large RAM).
# Skipped gracefully if allocation fails.
echo "--- pbfhogg add-locations-to-ways --index-type dense ---"
if time "$PBFHOGG" add-locations-to-ways "$INPUT" -o "$OUTDIR/pbfhogg-dense.osm.pbf" --index-type dense; then
    echo ""
    echo "=== Diff (hash vs dense) ==="
    "$PBFHOGG" diff --suppress-common "$OUTDIR/pbfhogg.osm.pbf" "$OUTDIR/pbfhogg-dense.osm.pbf"
    echo ""
else
    echo "Dense index allocation failed (expected on <128 GB systems without vm.overcommit_memory=1)"
    echo ""
fi

echo "Done."
