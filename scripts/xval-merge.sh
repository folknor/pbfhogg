#!/usr/bin/env bash
# Cross-validate merge output from 4 tools: pbfhogg, osmium, osmosis, osmconvert
# Usage: scripts/xval-merge.sh [base.osm.pbf] [changes.osc.gz]
set -euo pipefail
cd "$(dirname "$0")/.."

BASE="${1:-data/denmark-20260220-seq4704.osm.pbf}"
OSC="${2:-data/denmark-20260221-seq4705.osc.gz}"
OUTDIR="target/xval"
PBFHOGG="/media/folk/Hekkan/cargo/release/pbfhogg"
OSMOSIS_BIN="data/osmosis/osmosis-0.49.2/bin/osmosis"
export JAVA_HOME="$(pwd)/data/jdk"

mkdir -p "$OUTDIR"

echo "=== Cross-validation merge ==="
echo "Base: $BASE"
echo "Diff: $OSC"
echo ""

# Build pbfhogg first
scripts/build.sh

# --- Run all 4 tools ---
echo "--- pbfhogg merge ---"
time "$PBFHOGG" merge "$BASE" "$OSC" -o "$OUTDIR/pbfhogg.osm.pbf"
echo ""

echo "--- osmium apply-changes ---"
time osmium apply-changes "$BASE" "$OSC" -o "$OUTDIR/osmium.osm.pbf" --overwrite
echo ""

echo "--- osmosis --apply-change ---"
time "$OSMOSIS_BIN" \
    --read-xml-change file="$OSC" \
    --read-pbf file="$BASE" \
    --apply-change \
    --write-pbf file="$OUTDIR/osmosis.osm.pbf"
echo ""

echo "--- osmconvert ---"
time osmconvert "$BASE" "$OSC" -o="$OUTDIR/osmconvert.osm.pbf"
echo ""

# --- Count elements in each output ---
echo "=== Element counts ==="
for tool in pbfhogg osmium osmosis osmconvert; do
    f="$OUTDIR/$tool.osm.pbf"
    if [ -f "$f" ]; then
        echo "--- $tool ---"
        "$PBFHOGG" fileinfo --extended "$f"
        echo ""
    else
        echo "--- $tool --- MISSING"
    fi
done
