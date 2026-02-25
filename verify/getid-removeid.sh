#!/usr/bin/env bash
# Cross-validate getid and removeid: pbfhogg vs osmium getid
# Also validates removeid complement: getid + removeid = original
# Usage: verify/getid-removeid.sh [input.osm.pbf]
set -euo pipefail
cd "$(dirname "$0")/.."

INPUT="${1:-data/denmark-latest.osm.pbf}"
OUTDIR="target/verify/getid-removeid"
PBFHOGG="/media/folk/Hekkan/cargo/release/pbfhogg"

# IDs to extract (known to exist in Denmark extract)
IDS="n115722 n115723 n115724 w2080 w2081 w2082 r174 r213 r339"

mkdir -p "$OUTDIR"

echo "=== Cross-validation getid / removeid ==="
echo "Input: $INPUT"
echo "IDs: $IDS"
echo ""

# Build pbfhogg first
scripts/build.sh

# --- getid: pbfhogg vs osmium ---
echo "--- pbfhogg getid ---"
time "$PBFHOGG" getid "$INPUT" -o "$OUTDIR/pbfhogg-getid.osm.pbf" $IDS
echo ""

echo "--- osmium getid ---"
time osmium getid "$INPUT" $IDS -o "$OUTDIR/osmium-getid.osm.pbf" --overwrite
echo ""

echo "=== Element counts (getid) ==="
for tool in pbfhogg osmium; do
    f="$OUTDIR/$tool-getid.osm.pbf"
    if [ -f "$f" ]; then
        echo "--- $tool ---"
        "$PBFHOGG" fileinfo --extended "$f"
        echo ""
    else
        echo "--- $tool --- MISSING"
    fi
done

echo "=== Diff (getid, suppress-common) ==="
"$PBFHOGG" diff --suppress-common "$OUTDIR/pbfhogg-getid.osm.pbf" "$OUTDIR/osmium-getid.osm.pbf"
echo ""

# --- removeid: complement test ---
# osmium does not have removeid, so validate via complement:
# getid count + removeid count should equal original count
echo "--- pbfhogg removeid ---"
time "$PBFHOGG" removeid "$INPUT" -o "$OUTDIR/pbfhogg-removeid.osm.pbf" $IDS
echo ""

echo "=== Element counts (original vs getid + removeid) ==="
echo "--- original ---"
"$PBFHOGG" fileinfo --extended "$INPUT"
echo ""
echo "--- getid ---"
"$PBFHOGG" fileinfo --extended "$OUTDIR/pbfhogg-getid.osm.pbf"
echo ""
echo "--- removeid ---"
"$PBFHOGG" fileinfo --extended "$OUTDIR/pbfhogg-removeid.osm.pbf"
echo ""

# Verify complement: getid + removeid = original (counts printed above)

echo "Done."
