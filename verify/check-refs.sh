#!/usr/bin/env bash
# Cross-validate check-refs output: pbfhogg vs osmium
# Usage: verify/check-refs.sh [input.osm.pbf]
set -euo pipefail
cd "$(dirname "$0")/.."

INPUT="${1:-data/denmark-latest.osm.pbf}"
OUTDIR="target/verify/check-refs"

source "$(dirname "$0")/lib.sh"
mkdir -p "$OUTDIR"

echo "=== Cross-validation check-refs ==="
echo "Input: $INPUT"
echo ""

# Build pbfhogg first
scripts/build.sh

# --- Without relation checks (default for osmium) ---
echo "--- pbfhogg check-refs (ways only) ---"
time "$PBFHOGG" check-refs "$INPUT" > "$OUTDIR/pbfhogg-ways.txt" 2>&1 || true
echo ""

echo "--- osmium check-refs (ways only) ---"
time osmium check-refs "$INPUT" > "$OUTDIR/osmium-ways.txt" 2>&1 || true
echo ""

echo "=== pbfhogg output (ways only) ==="
cat "$OUTDIR/pbfhogg-ways.txt"
echo ""

echo "=== osmium output (ways only) ==="
cat "$OUTDIR/osmium-ways.txt"
echo ""

# --- With relation checks ---
echo "--- pbfhogg check-refs --check-relations ---"
time "$PBFHOGG" check-refs "$INPUT" --check-relations > "$OUTDIR/pbfhogg-all.txt" 2>&1 || true
echo ""

echo "--- osmium check-refs -r ---"
time osmium check-refs -r "$INPUT" > "$OUTDIR/osmium-all.txt" 2>&1 || true
echo ""

echo "=== pbfhogg output (with relations) ==="
cat "$OUTDIR/pbfhogg-all.txt"
echo ""

echo "=== osmium output (with relations) ==="
cat "$OUTDIR/osmium-all.txt"
echo ""

echo "Done."
