#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
source "$(dirname "$0")/lib.sh"

# Germany dataset for scale profiling (~10× Denmark).
PBF_ORIG="data/germany-20260224-seq4704.osm.pbf"
PBF_IDX="data/germany-20260224-seq4704-with-indexdata.osm.pbf"
OSC="data/germany-20260225-seq4705.osc.gz"

for f in "$PBF_ORIG" "$PBF_IDX" "$OSC"; do
    if [ ! -f "$f" ]; then
        echo "Missing: $f"
        exit 1
    fi
done

cargo build --release -p pbfhogg-cli --features hotpath

OUTFILE=$(mktemp "$CARGO_TARGET_DIR/.hotpath_germany.XXXXXX")
MERGED=$(mktemp "$CARGO_TARGET_DIR/.hotpath_germany_merged.XXXXXX.osm.pbf")
trap 'rm -f "$OUTFILE" "$MERGED"' EXIT
BIN="$PBFHOGG"

FILE_MB=$(file_size_mb "$PBF_IDX")
echo "=== Germany hotpath ($FILE_MB MB, with indexdata) ==="
echo ""

# 1. Merge: no indexdata, zlib (old baseline)
echo "--- merge: no indexdata, zlib ---"
HOTPATH_METRICS_SERVER_OFF=true "$BIN" merge "$PBF_ORIG" "$OSC" --compression zlib -o "$MERGED" > "$OUTFILE"
grep -A 1000 '^\[hotpath\]' "$OUTFILE"

echo ""

# 2. Merge: indexdata, zlib
echo "--- merge: indexdata, zlib ---"
HOTPATH_METRICS_SERVER_OFF=true "$BIN" merge "$PBF_IDX" "$OSC" --compression zlib -o "$MERGED" > "$OUTFILE"
grep -A 1000 '^\[hotpath\]' "$OUTFILE"

echo ""

# 3. Merge: indexdata, none (nidhogg production path)
echo "--- merge: indexdata, none (nidhogg production) ---"
HOTPATH_METRICS_SERVER_OFF=true "$BIN" merge "$PBF_IDX" "$OSC" --compression none -o "$MERGED" > "$OUTFILE"
grep -A 1000 '^\[hotpath\]' "$OUTFILE"
