#!/usr/bin/env bash
# Cross-validate add-locations-to-ways: pbfhogg vs osmium
# Usage: verify/add-locations-to-ways.sh [input.osm.pbf]
set -euo pipefail
cd "$(dirname "$0")/.."

INPUT="${1:-data/denmark-latest.osm.pbf}"
OUTDIR="target/verify/add-locations-to-ways"
PBFHOGG="/media/folk/Hekkan/cargo/release/pbfhogg"

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

echo "Done."
