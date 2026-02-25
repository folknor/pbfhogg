#!/usr/bin/env bash
# Cross-validate sort output: pbfhogg sort vs osmium sort
# Usage: verify/sort.sh [input.osm.pbf]
set -euo pipefail
cd "$(dirname "$0")/.."

INPUT="${1:-data/denmark-latest.osm.pbf}"
OUTDIR="target/verify/sort"
PBFHOGG="/media/folk/Hekkan/cargo/release/pbfhogg"

mkdir -p "$OUTDIR"

echo "=== Cross-validation sort ==="
echo "Input: $INPUT"
echo ""

# Build pbfhogg first
scripts/build.sh

# --- Run both tools ---
echo "--- pbfhogg sort ---"
time "$PBFHOGG" sort "$INPUT" -o "$OUTDIR/pbfhogg.osm.pbf"
echo ""

echo "--- osmium sort ---"
time osmium sort "$INPUT" -o "$OUTDIR/osmium.osm.pbf" --overwrite
echo ""

# --- Element counts ---
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

# --- Diff ---
echo "=== pbfhogg diff (suppress-common) ==="
"$PBFHOGG" diff --suppress-common "$OUTDIR/pbfhogg.osm.pbf" "$OUTDIR/osmium.osm.pbf"
echo ""
echo "Done."
