#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

PBF="${1:-data/denmark-latest.osm.pbf}"
OSC="${2:-}"
COMPRESSION="${3:-zlib}"

if [ ! -f "$PBF" ]; then
    echo "PBF not found: $PBF"
    echo "Usage: scripts/run-hotpath.sh [pbf] [osc.gz] [compression]"
    echo "  compression: none, zlib (default), zlib:9, zstd, zstd:19, etc."
    exit 1
fi

cargo build --release -p pbfhogg-cli --features hotpath

# hotpath metrics go to stdout; capture to tempfile so command output
# doesn't bury them. stderr (summaries) flows through normally.
OUTFILE=$(mktemp .hotpath_out.XXXXXX)
MERGED=$(mktemp .hotpath_merged.XXXXXX.osm.pbf)
trap 'rm -f "$OUTFILE" "$MERGED"' EXIT
BIN=./target/release/pbfhogg

# 1. Pipelined read — ElementReader::for_each_pipelined
#    Same API path as elivagar and nidhogg ingest.
echo "--- pipelined read (tags-count) ---"
HOTPATH_METRICS_SERVER_OFF=true "$BIN" tags-count "$PBF" > "$OUTFILE"
grep -A 1000 '^\[hotpath\]' "$OUTFILE"

# 2. Full decode + write — BlockBuilder + PbfWriter
#    cat with type filter forces decode of every element and rebuild through
#    BlockBuilder, exercising the same write path as nidhogg output.
echo ""
echo "--- decode + write (cat --type, compression=$COMPRESSION) ---"
HOTPATH_METRICS_SERVER_OFF=true "$BIN" cat "$PBF" --type node,way,relation --compression "$COMPRESSION" -o /dev/null > "$OUTFILE"
grep -A 1000 '^\[hotpath\]' "$OUTFILE"

# 3. Merge — pbfhogg::merge::merge
#    Same API path as nidhogg weekly planet refresh (base PBF + OSC diffs).
#    Only runs if an OSC file is provided.
if [ -n "$OSC" ]; then
    if [ ! -f "$OSC" ]; then
        echo "OSC not found: $OSC"
        exit 1
    fi
    echo ""
    echo "--- merge (compression=$COMPRESSION) ---"
    HOTPATH_METRICS_SERVER_OFF=true "$BIN" merge "$PBF" "$OSC" --compression "$COMPRESSION" -o "$MERGED" > "$OUTFILE"
    grep -A 1000 '^\[hotpath\]' "$OUTFILE"
fi
