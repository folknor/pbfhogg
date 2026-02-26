#!/usr/bin/env bash
# Cross-validate cat output: pbfhogg cat vs osmium cat
# Usage: verify/cat.sh [input.osm.pbf]
set -euo pipefail
cd "$(dirname "$0")/.."

source "$(dirname "$0")/lib.sh"

INPUT="${1:-data/denmark-latest.osm.pbf}"
OUTDIR="$CARGO_TARGET_DIR/verify/cat"
mkdir -p "$OUTDIR"

echo "=== Cross-validation cat ==="
echo "Input: $INPUT"
echo ""

# Build pbfhogg first
scripts/build.sh

for TYPE in node way relation; do
    echo "--- pbfhogg cat -t $TYPE ---"
    time "$PBFHOGG" cat "$INPUT" -t "$TYPE" -o "$OUTDIR/pbfhogg-$TYPE.osm.pbf"
    echo ""

    echo "--- osmium cat -t $TYPE ---"
    time osmium cat "$INPUT" -t "$TYPE" -o "$OUTDIR/osmium-$TYPE.osm.pbf" --overwrite
    echo ""

    echo "=== Element counts ($TYPE) ==="
    for tool in pbfhogg osmium; do
        f="$OUTDIR/$tool-$TYPE.osm.pbf"
        if [ -f "$f" ]; then
            echo "--- $tool ---"
            "$PBFHOGG" fileinfo --extended "$f"
            echo ""
        else
            echo "--- $tool --- MISSING"
        fi
    done

    echo "=== Diff ($TYPE, suppress-common) ==="
    "$PBFHOGG" diff --suppress-common "$OUTDIR/pbfhogg-$TYPE.osm.pbf" "$OUTDIR/osmium-$TYPE.osm.pbf"
    echo ""

    echo "=== Sort.Type_then_ID check ($TYPE) ==="
    compare_sort_feature "$OUTDIR/pbfhogg-$TYPE.osm.pbf" "$OUTDIR/osmium-$TYPE.osm.pbf"
    echo ""
done

echo "Done."
