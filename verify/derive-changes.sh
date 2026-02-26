#!/usr/bin/env bash
# Cross-validate derive-changes output: pbfhogg vs osmium
# Creates "new" PBF by applying an OSC to the base, then derives changes back.
# Roundtrip: apply each tool's derived OSC back to old, compare resulting PBFs.
# Usage: verify/derive-changes.sh [old.osm.pbf] [changes.osc.gz]
set -euo pipefail
cd "$(dirname "$0")/.."

OLD="${1:-data/denmark-20260220-seq4704.osm.pbf}"
OSC="${2:-data/denmark-20260221-seq4705.osc.gz}"
OUTDIR="target/verify/derive-changes"
PBFHOGG="/media/folk/Hekkan/cargo/release/pbfhogg"

source "$(dirname "$0")/lib.sh"
mkdir -p "$OUTDIR"

echo "=== Cross-validation derive-changes ==="
echo "Old: $OLD"
echo "OSC: $OSC (used to create 'new' via merge)"
echo ""

# Build pbfhogg first
scripts/build.sh

# Step 1: Create "new" PBF by applying the OSC
echo "--- Creating 'new' PBF via merge ---"
time "$PBFHOGG" merge "$OLD" "$OSC" -o "$OUTDIR/new.osm.pbf"
echo ""

# Step 2: Derive changes with both tools
echo "--- pbfhogg derive-changes ---"
time "$PBFHOGG" derive-changes "$OLD" "$OUTDIR/new.osm.pbf" -o "$OUTDIR/pbfhogg.osc.gz"
echo ""

echo "--- osmium derive-changes ---"
time osmium derive-changes "$OLD" "$OUTDIR/new.osm.pbf" -o "$OUTDIR/osmium.osc.gz" --overwrite
echo ""

# Step 3: File sizes
echo "=== OSC file sizes ==="
ls -lh "$OUTDIR/pbfhogg.osc.gz"
ls -lh "$OUTDIR/osmium.osc.gz"
echo ""

# Step 4: Roundtrip — apply each OSC back to old, compare resulting PBFs
echo "--- Roundtrip: apply pbfhogg OSC ---"
time "$PBFHOGG" merge "$OLD" "$OUTDIR/pbfhogg.osc.gz" -o "$OUTDIR/roundtrip-pbfhogg.osm.pbf"
echo ""

echo "--- Roundtrip: apply osmium OSC ---"
time osmium apply-changes "$OLD" "$OUTDIR/osmium.osc.gz" -o "$OUTDIR/roundtrip-osmium.osm.pbf" --overwrite
echo ""

echo "=== Element counts ==="
for label in new roundtrip-pbfhogg roundtrip-osmium; do
    f="$OUTDIR/$label.osm.pbf"
    if [ -f "$f" ]; then
        echo "--- $label ---"
        "$PBFHOGG" fileinfo --extended "$f"
        echo ""
    else
        echo "--- $label --- MISSING"
    fi
done

echo "=== Diff: pbfhogg roundtrip vs new ==="
"$PBFHOGG" diff --suppress-common "$OUTDIR/roundtrip-pbfhogg.osm.pbf" "$OUTDIR/new.osm.pbf" || true
echo ""

echo "=== Diff: osmium roundtrip vs new ==="
"$PBFHOGG" diff --suppress-common "$OUTDIR/roundtrip-osmium.osm.pbf" "$OUTDIR/new.osm.pbf" || true
echo ""

echo "=== Diff: pbfhogg vs osmium roundtrips ==="
"$PBFHOGG" diff --suppress-common "$OUTDIR/roundtrip-pbfhogg.osm.pbf" "$OUTDIR/roundtrip-osmium.osm.pbf" || true
echo ""

echo "=== Sort.Type_then_ID check ==="
assert_sorted "$OUTDIR/new.osm.pbf" "pbfhogg merge (new)"
assert_sorted "$OUTDIR/roundtrip-pbfhogg.osm.pbf" "pbfhogg merge (roundtrip)"
echo ""

echo "Done."
