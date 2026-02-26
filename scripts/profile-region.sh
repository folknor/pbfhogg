#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
source "$(dirname "$0")/lib.sh"

# Usage: scripts/profile-region.sh <name> <pbf-original> <osc>
#
# Runs the full timing + alloc profiling suite for a region.
# Generates the indexed PBF variant if it doesn't exist.
# Output goes to stdout — redirect to capture.
#
# Example:
#   scripts/profile-region.sh denmark data/denmark.osm.pbf data/denmark.osc.gz

if [ $# -lt 3 ]; then
    echo "Usage: $0 <name> <pbf-original> <osc>"
    echo ""
    echo "  name          Region name (for labeling)"
    echo "  pbf-original  Path to original PBF (from Geofabrik, no indexdata)"
    echo "  osc           Path to OSC diff file"
    exit 1
fi

NAME="$1"
PBF_ORIG="$2"
OSC="$3"

for f in "$PBF_ORIG" "$OSC"; do
    if [ ! -f "$f" ]; then
        echo "Missing: $f"
        exit 1
    fi
done

# Derive indexed PBF path: insert "-with-indexdata" before .osm.pbf
PBF_IDX="${PBF_ORIG%.osm.pbf}-with-indexdata.osm.pbf"

ORIG_MB=$(file_size_mb "$PBF_ORIG")

# Generate indexed variant if needed
if [ ! -f "$PBF_IDX" ]; then
    echo "=== Generating indexed PBF: $PBF_IDX ==="
    cargo build --release -p pbfhogg-cli
    "$PBFHOGG" cat "$PBF_ORIG" --type node,way,relation -o "$PBF_IDX"
    echo ""
fi

IDX_MB=$(file_size_mb "$PBF_IDX")

echo "========================================"
echo "=== $NAME ($ORIG_MB MB original, $IDX_MB MB indexed) ==="
echo "========================================"
echo ""

# --- TIMING PASS ---

cargo build --release -p pbfhogg-cli --features hotpath

OUTFILE=$(mktemp "$CARGO_TARGET_DIR/.profile_${NAME}.XXXXXX")
MERGED=$(mktemp "$CARGO_TARGET_DIR/.profile_${NAME}_merged.XXXXXX.osm.pbf")
trap 'rm -f "$OUTFILE" "$MERGED"' EXIT
BIN="$PBFHOGG"

echo "--- tags-count (pipelined read, indexed PBF) ---"
HOTPATH_METRICS_SERVER_OFF=true "$BIN" tags-count "$PBF_IDX" > "$OUTFILE"
grep -A 1000 '^\[hotpath\]' "$OUTFILE"
echo ""

echo "--- check-refs (pipelined read, indexed PBF) ---"
HOTPATH_METRICS_SERVER_OFF=true "$BIN" check-refs "$PBF_IDX" > "$OUTFILE" 2>&1
grep -A 1000 '^\[hotpath\]' "$OUTFILE"
echo ""

echo "--- cat --type node,way,relation (decode+write, zlib, indexed PBF) ---"
HOTPATH_METRICS_SERVER_OFF=true "$BIN" cat "$PBF_IDX" --type node,way,relation --compression zlib -o /dev/null > "$OUTFILE"
grep -A 1000 '^\[hotpath\]' "$OUTFILE"
echo ""

echo "--- merge: no indexdata, zlib ---"
HOTPATH_METRICS_SERVER_OFF=true "$BIN" merge "$PBF_ORIG" "$OSC" --compression zlib -o "$MERGED" > "$OUTFILE"
grep -A 1000 '^\[hotpath\]' "$OUTFILE"
echo ""

echo "--- merge: indexdata, zlib ---"
HOTPATH_METRICS_SERVER_OFF=true "$BIN" merge "$PBF_IDX" "$OSC" --compression zlib -o "$MERGED" > "$OUTFILE"
grep -A 1000 '^\[hotpath\]' "$OUTFILE"
echo ""

echo "--- merge: indexdata, none ---"
HOTPATH_METRICS_SERVER_OFF=true "$BIN" merge "$PBF_IDX" "$OSC" --compression none -o "$MERGED" > "$OUTFILE"
grep -A 1000 '^\[hotpath\]' "$OUTFILE"
echo ""

# --- ALLOC PASS ---

echo "========================================"
echo "=== $NAME ALLOCATIONS ==="
echo "========================================"
echo ""

cargo build --release -p pbfhogg-cli --features hotpath-alloc
BIN="$PBFHOGG"

echo "--- cat --type (alloc, indexed PBF) ---"
HOTPATH_METRICS_SERVER_OFF=true "$BIN" cat "$PBF_IDX" --type node,way,relation --compression zlib -o /dev/null > "$OUTFILE"
grep -A 1000 '^\[hotpath\]' "$OUTFILE"
echo ""

echo "--- merge: indexdata, none (alloc) ---"
HOTPATH_METRICS_SERVER_OFF=true "$BIN" merge "$PBF_IDX" "$OSC" --compression none -o "$MERGED" > "$OUTFILE"
grep -A 1000 '^\[hotpath\]' "$OUTFILE"
echo ""

echo "=== $NAME COMPLETE ==="
