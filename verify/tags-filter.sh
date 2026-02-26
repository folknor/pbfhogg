#!/usr/bin/env bash
# Cross-validate tags-filter output: pbfhogg vs osmium
# Usage: verify/tags-filter.sh [input.osm.pbf]
set -euo pipefail
cd "$(dirname "$0")/.."

INPUT="${1:-data/denmark-latest.osm.pbf}"
OUTDIR="target/verify/tags-filter"

source "$(dirname "$0")/lib.sh"
mkdir -p "$OUTDIR"

echo "=== Cross-validation tags-filter ==="
echo "Input: $INPUT"
echo ""

# Build pbfhogg first
scripts/build.sh

# --- Test 1: highway=primary with omit-referenced (simplest, no ref-following) ---
echo "--- pbfhogg tags-filter highway=primary -R ---"
time "$PBFHOGG" tags-filter "$INPUT" -o "$OUTDIR/pbfhogg-highway-R.osm.pbf" -R "highway=primary"
echo ""

echo "--- osmium tags-filter highway=primary -R ---"
time osmium tags-filter "$INPUT" "highway=primary" -R -o "$OUTDIR/osmium-highway-R.osm.pbf" --overwrite
echo ""

echo "=== Element counts (highway=primary -R) ==="
for tool in pbfhogg osmium; do
    f="$OUTDIR/$tool-highway-R.osm.pbf"
    if [ -f "$f" ]; then
        echo "--- $tool ---"
        "$PBFHOGG" fileinfo --extended "$f"
        echo ""
    else
        echo "--- $tool --- MISSING"
    fi
done

echo "=== Diff (highway=primary -R, suppress-common) ==="
"$PBFHOGG" diff --suppress-common "$OUTDIR/pbfhogg-highway-R.osm.pbf" "$OUTDIR/osmium-highway-R.osm.pbf"
echo ""

echo "=== Sort.Type_then_ID check (highway=primary -R) ==="
compare_sort_feature "$OUTDIR/pbfhogg-highway-R.osm.pbf" "$OUTDIR/osmium-highway-R.osm.pbf"
echo ""

# --- Test 2: amenity=restaurant with omit-referenced ---
echo "--- pbfhogg tags-filter amenity=restaurant -R ---"
time "$PBFHOGG" tags-filter "$INPUT" -o "$OUTDIR/pbfhogg-amenity-R.osm.pbf" -R "amenity=restaurant"
echo ""

echo "--- osmium tags-filter amenity=restaurant -R ---"
time osmium tags-filter "$INPUT" "amenity=restaurant" -R -o "$OUTDIR/osmium-amenity-R.osm.pbf" --overwrite
echo ""

echo "=== Element counts (amenity=restaurant -R) ==="
for tool in pbfhogg osmium; do
    f="$OUTDIR/$tool-amenity-R.osm.pbf"
    if [ -f "$f" ]; then
        echo "--- $tool ---"
        "$PBFHOGG" fileinfo --extended "$f"
        echo ""
    else
        echo "--- $tool --- MISSING"
    fi
done

echo "=== Diff (amenity=restaurant -R, suppress-common) ==="
"$PBFHOGG" diff --suppress-common "$OUTDIR/pbfhogg-amenity-R.osm.pbf" "$OUTDIR/osmium-amenity-R.osm.pbf"
echo ""

echo "=== Sort.Type_then_ID check (amenity=restaurant -R) ==="
compare_sort_feature "$OUTDIR/pbfhogg-amenity-R.osm.pbf" "$OUTDIR/osmium-amenity-R.osm.pbf"
echo ""

# --- Test 3: w/highway=primary (type-prefixed, ways only) with omit-referenced ---
echo "--- pbfhogg tags-filter w/highway=primary -R ---"
time "$PBFHOGG" tags-filter "$INPUT" -o "$OUTDIR/pbfhogg-w-highway-R.osm.pbf" -R "w/highway=primary"
echo ""

echo "--- osmium tags-filter w/highway=primary -R ---"
time osmium tags-filter "$INPUT" "w/highway=primary" -R -o "$OUTDIR/osmium-w-highway-R.osm.pbf" --overwrite
echo ""

echo "=== Element counts (w/highway=primary -R) ==="
for tool in pbfhogg osmium; do
    f="$OUTDIR/$tool-w-highway-R.osm.pbf"
    if [ -f "$f" ]; then
        echo "--- $tool ---"
        "$PBFHOGG" fileinfo --extended "$f"
        echo ""
    else
        echo "--- $tool --- MISSING"
    fi
done

echo "=== Diff (w/highway=primary -R, suppress-common) ==="
"$PBFHOGG" diff --suppress-common "$OUTDIR/pbfhogg-w-highway-R.osm.pbf" "$OUTDIR/osmium-w-highway-R.osm.pbf"
echo ""

echo "=== Sort.Type_then_ID check (w/highway=primary -R) ==="
compare_sort_feature "$OUTDIR/pbfhogg-w-highway-R.osm.pbf" "$OUTDIR/osmium-w-highway-R.osm.pbf"
echo ""

echo "Done."
