#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

# Benchmark extract strategies on Japan with a Tokyo-area bbox.
# Usage: scripts/bench-extract-japan.sh [strategy] [runs]
# strategy: simple, complete, smart (default: all three sequentially)
# runs: number of runs, best-of (default: 3)

PBF="data/japan-20260225-seq4706.osm.pbf"
BBOX="139.5,35.5,140.0,36.0"  # Tokyo metro area
RUNS="${2:-3}"
STRATEGY="${1:-all}"
BIN="target/release/pbfhogg"

if [ ! -f "$PBF" ]; then
    echo "Japan PBF not found: $PBF"
    exit 1
fi

echo "=== Japan extract benchmark ==="
echo "  file: $PBF"
echo "  bbox: $BBOX (Tokyo area)"
echo "  runs: $RUNS (best of)"
echo ""

cargo build --release 2>&1 | tail -2

best_of() {
    local label="$1"
    shift
    local best=""
    for i in $(seq 1 "$RUNS"); do
        local t
        t=$( { time "$@" > /dev/null 2>&1 ; } 2>&1 | grep real | sed 's/real\t//' | sed 's/,/./' | sed 's/s$//' )
        # Parse minutes and seconds
        local mins secs
        mins=$(echo "$t" | sed 's/m.*//')
        secs=$(echo "$t" | sed 's/.*m//')
        local total
        total=$(echo "$mins * 60 + $secs" | bc)
        if [ -z "$best" ]; then
            best="$total"
        else
            local is_less
            is_less=$(echo "$total < $best" | bc)
            if [ "$is_less" -eq 1 ]; then
                best="$total"
            fi
        fi
    done
    printf "  %-40s %ss\n" "$label" "$best"
}

run_strategy() {
    local strat="$1"
    case "$strat" in
        simple)
            echo "--- extract --simple ---"
            best_of "pbfhogg" "$BIN" extract "$PBF" --simple -b "$BBOX" -o /dev/null
            best_of "osmium" osmium extract "$PBF" -s simple -b "$BBOX" -o /tmp/osmium-japan-extract.pbf --overwrite
            echo ""
            ;;
        complete)
            echo "--- extract (complete-ways, default) ---"
            best_of "pbfhogg" "$BIN" extract "$PBF" -b "$BBOX" -o /dev/null
            best_of "osmium" osmium extract "$PBF" -s complete_ways -b "$BBOX" -o /tmp/osmium-japan-extract.pbf --overwrite
            echo ""
            ;;
        smart)
            echo "--- extract --smart ---"
            best_of "pbfhogg" "$BIN" extract "$PBF" --smart -b "$BBOX" -o /dev/null
            best_of "osmium" osmium extract "$PBF" -s smart -b "$BBOX" -o /tmp/osmium-japan-extract.pbf --overwrite
            echo ""
            ;;
    esac
}

if [ "$STRATEGY" = "all" ]; then
    run_strategy simple
    run_strategy complete
    run_strategy smart
else
    run_strategy "$STRATEGY"
fi

rm -f /tmp/osmium-japan-extract.pbf
