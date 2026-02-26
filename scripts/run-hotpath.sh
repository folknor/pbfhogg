#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
source "$(dirname "$0")/lib.sh"

# Fixed dataset for reproducible profiling.
PBF="data/denmark-20260220-seq4704-with-indexdata.osm.pbf"
OSC="data/denmark-20260221-seq4705.osc.gz"
COMPRESSION="zlib"

if [ ! -f "$PBF" ]; then
    echo "PBF not found: $PBF"
    exit 1
fi

cargo build --release -p pbfhogg-cli --features hotpath

# hotpath metrics go to stdout; capture to tempfile so command output
# doesn't bury them. stderr (summaries) flows through normally.
OUTFILE=$(mktemp "$CARGO_TARGET_DIR/.hotpath_out.XXXXXX")
MERGED=$(mktemp "$CARGO_TARGET_DIR/.hotpath_merged.XXXXXX.osm.pbf")
trap 'rm -f "$OUTFILE" "$MERGED"' EXIT
BIN="$PBFHOGG"

# 1. Pipelined read — ElementReader::for_each_pipelined
#    Same API path as elivagar and nidhogg ingest.
echo "--- pipelined read (tags-count) ---"
HOTPATH_METRICS_SERVER_OFF=true "$BIN" tags-count "$PBF" > "$OUTFILE"
grep -A 1000 '^\[hotpath\]' "$OUTFILE"

# 2. Check-refs — pipelined read, lightweight processing
#    Comparable to TODO.md baseline numbers.
echo ""
echo "--- pipelined read (check-refs) ---"
HOTPATH_METRICS_SERVER_OFF=true "$BIN" check-refs "$PBF" > "$OUTFILE" 2>&1
grep -A 1000 '^\[hotpath\]' "$OUTFILE"

# 3. Full decode + write — BlockBuilder + PbfWriter
#    cat with type filter forces decode of every element and rebuild through
#    BlockBuilder, exercising the same write path as nidhogg output.
echo ""
echo "--- decode + write (cat --type, compression=$COMPRESSION) ---"
HOTPATH_METRICS_SERVER_OFF=true "$BIN" cat "$PBF" --type node,way,relation --compression "$COMPRESSION" -o /dev/null > "$OUTFILE"
grep -A 1000 '^\[hotpath\]' "$OUTFILE"

# 4. Merge — pbfhogg::merge::merge
#    Same API path as nidhogg weekly planet refresh (base PBF + OSC diffs).
echo ""
echo "--- merge (compression=$COMPRESSION) ---"
HOTPATH_METRICS_SERVER_OFF=true "$BIN" merge "$PBF" "$OSC" --compression "$COMPRESSION" -o "$MERGED" > "$OUTFILE"
grep -A 1000 '^\[hotpath\]' "$OUTFILE"
