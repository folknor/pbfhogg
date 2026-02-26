#!/usr/bin/env bash
# Cross-validate extract output: pbfhogg extract vs osmium extract
# Usage: verify/extract.sh [input.osm.pbf]
set -euo pipefail
cd "$(dirname "$0")/.."

source "$(dirname "$0")/lib.sh"

INPUT="${1:-data/denmark-latest.osm.pbf}"
BBOX="12.4,55.6,12.7,55.8"  # Copenhagen area
OUTDIR="$CARGO_TARGET_DIR/verify/extract"
mkdir -p "$OUTDIR"

echo "=== Cross-validation extract ==="
echo "Input: $INPUT"
echo "Bbox: $BBOX"
echo ""

# Build pbfhogg first
scripts/build.sh

# --- Simple strategy ---
echo "--- pbfhogg extract --simple ---"
time "$PBFHOGG" extract "$INPUT" -o "$OUTDIR/pbfhogg-simple.osm.pbf" -b "$BBOX" --simple
echo ""

echo "--- osmium extract -s simple ---"
time osmium extract "$INPUT" -o "$OUTDIR/osmium-simple.osm.pbf" -b "$BBOX" -s simple --overwrite
echo ""

echo "=== Element counts (simple) ==="
for tool in pbfhogg osmium; do
    f="$OUTDIR/$tool-simple.osm.pbf"
    if [ -f "$f" ]; then
        echo "--- $tool ---"
        "$PBFHOGG" fileinfo --extended "$f"
        echo ""
    else
        echo "--- $tool --- MISSING"
    fi
done

echo "=== Diff (simple, suppress-common) ==="
"$PBFHOGG" diff --suppress-common "$OUTDIR/pbfhogg-simple.osm.pbf" "$OUTDIR/osmium-simple.osm.pbf" || true
echo ""

echo "=== Sort.Type_then_ID check (simple) ==="
assert_sorted "$OUTDIR/pbfhogg-simple.osm.pbf" "pbfhogg extract --simple"
echo ""

# --- Complete-ways strategy ---
echo "--- pbfhogg extract (complete-ways) ---"
time "$PBFHOGG" extract "$INPUT" -o "$OUTDIR/pbfhogg-complete.osm.pbf" -b "$BBOX"
echo ""

echo "--- osmium extract -s complete_ways ---"
time osmium extract "$INPUT" -o "$OUTDIR/osmium-complete.osm.pbf" -b "$BBOX" -s complete_ways --overwrite
echo ""

echo "=== Element counts (complete-ways) ==="
for tool in pbfhogg osmium; do
    f="$OUTDIR/$tool-complete.osm.pbf"
    if [ -f "$f" ]; then
        echo "--- $tool ---"
        "$PBFHOGG" fileinfo --extended "$f"
        echo ""
    else
        echo "--- $tool --- MISSING"
    fi
done

echo "=== Diff (complete-ways, suppress-common) ==="
"$PBFHOGG" diff --suppress-common "$OUTDIR/pbfhogg-complete.osm.pbf" "$OUTDIR/osmium-complete.osm.pbf" || true
echo ""

echo "=== Sort.Type_then_ID check (complete-ways) ==="
assert_sorted "$OUTDIR/pbfhogg-complete.osm.pbf" "pbfhogg extract complete-ways"
echo ""

# --- Smart strategy ---
echo "--- pbfhogg extract --smart ---"
time "$PBFHOGG" extract "$INPUT" -o "$OUTDIR/pbfhogg-smart.osm.pbf" -b "$BBOX" --smart
echo ""

echo "--- osmium extract -s smart ---"
time osmium extract "$INPUT" -o "$OUTDIR/osmium-smart.osm.pbf" -b "$BBOX" -s smart --overwrite
echo ""

echo "=== Element counts (smart) ==="
for tool in pbfhogg osmium; do
    f="$OUTDIR/$tool-smart.osm.pbf"
    if [ -f "$f" ]; then
        echo "--- $tool ---"
        "$PBFHOGG" fileinfo --extended "$f"
        echo ""
    else
        echo "--- $tool --- MISSING"
    fi
done

echo "=== Diff (smart, suppress-common) ==="
"$PBFHOGG" diff --suppress-common "$OUTDIR/pbfhogg-smart.osm.pbf" "$OUTDIR/osmium-smart.osm.pbf" || true
echo ""

echo "=== Sort.Type_then_ID check (smart) ==="
assert_sorted "$OUTDIR/pbfhogg-smart.osm.pbf" "pbfhogg extract --smart"
echo ""

echo "Done."
