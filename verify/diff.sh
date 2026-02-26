#!/usr/bin/env bash
# Cross-validate diff output: pbfhogg diff vs osmium diff
# Creates "new" PBF by applying an OSC to the base, then diffs old vs new.
# Usage: verify/diff.sh [old.osm.pbf] [changes.osc.gz]
set -euo pipefail
cd "$(dirname "$0")/.."

source "$(dirname "$0")/lib.sh"

OLD="${1:-data/denmark-20260220-seq4704.osm.pbf}"
OSC="${2:-data/denmark-20260221-seq4705.osc.gz}"
OUTDIR="$CARGO_TARGET_DIR/verify/diff"
mkdir -p "$OUTDIR"

echo "=== Cross-validation diff ==="
echo "Old: $OLD"
echo "OSC: $OSC (used to create 'new' via merge)"
echo ""

# Build pbfhogg first
scripts/build.sh

# Create "new" PBF by applying the OSC
echo "--- Creating 'new' PBF via merge ---"
time "$PBFHOGG" merge "$OLD" "$OSC" -o "$OUTDIR/new.osm.pbf"
echo ""

# Run both diff tools (both exit non-zero when differences exist)
echo "--- pbfhogg diff ---"
time "$PBFHOGG" diff -c "$OLD" "$OUTDIR/new.osm.pbf" > "$OUTDIR/pbfhogg-diff.txt" 2>"$OUTDIR/pbfhogg-summary.txt" || true
echo ""

echo "--- osmium diff ---"
time osmium diff "$OLD" "$OUTDIR/new.osm.pbf" --summary > "$OUTDIR/osmium-diff.txt" 2>"$OUTDIR/osmium-summary.txt" || true
echo ""

# Print summaries
echo "=== pbfhogg diff summary ==="
cat "$OUTDIR/pbfhogg-summary.txt"
echo ""

echo "=== osmium diff summary ==="
cat "$OUTDIR/osmium-summary.txt"
echo ""

# Line counts
echo "=== Output line counts ==="
echo "pbfhogg: $(wc -l < "$OUTDIR/pbfhogg-diff.txt") lines"
echo "osmium:  $(wc -l < "$OUTDIR/osmium-diff.txt") lines"
echo ""

echo "Done."
