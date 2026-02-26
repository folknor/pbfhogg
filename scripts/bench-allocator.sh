#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
source "$(dirname "$0")/lib.sh"

PBF="${1:-data/denmark-20260220-seq4704.osm.pbf}"
RUNS="${2:-3}"

echo "=== Allocator benchmark: check-refs on $(basename "$PBF") ==="
echo "Runs per allocator: $RUNS"
echo ""

for ALLOC in default jemalloc mimalloc; do
    if [ "$ALLOC" = "default" ]; then
        FEATURES=""
    else
        FEATURES="--features $ALLOC"
    fi

    echo "--- Building with allocator: $ALLOC ---"
    cargo build --release -p pbfhogg-cli $FEATURES 2>&1

    echo "--- Running $ALLOC ($RUNS runs) ---"
    for i in $(seq 1 "$RUNS"); do
        echo "  Run $i/$RUNS:"
        HOTPATH_METRICS_SERVER_OFF=true "$PBFHOGG" check-refs "$PBF" 2>&1
        echo ""
    done
done
