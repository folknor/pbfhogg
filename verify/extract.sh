#!/usr/bin/env bash
# Cross-validate extract output: pbfhogg extract vs osmium extract
# Usage: verify/extract.sh [input.osm.pbf]
set -euo pipefail
cd "$(dirname "$0")/.."

INPUT="${1:-data/denmark-latest.osm.pbf}"
OUTDIR="target/verify/extract"
PBFHOGG="/media/folk/Hekkan/cargo/release/pbfhogg"
BBOX="12.4,55.6,12.7,55.8"  # Copenhagen area

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

echo "Done."
