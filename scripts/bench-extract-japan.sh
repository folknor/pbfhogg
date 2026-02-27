#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
source "$(dirname "$0")/lib.sh"

# Benchmark extract strategies on Japan with a Tokyo-area bbox.
# Usage: scripts/bench-extract-japan.sh [strategy] [runs]
# strategy: simple, complete, smart (default: all three sequentially)
# runs: number of runs, best-of (default: 3)

PBF="data/japan-20260225-seq4706.osm.pbf"
BBOX="139.5,35.5,140.0,36.0"  # Tokyo metro area
RUNS="${2:-3}"
STRATEGY="${1:-all}"
BIN="$PBFHOGG"
OSMIUM_OUT="$CARGO_TARGET_DIR/bench-extract-japan.osm.pbf"

if [ ! -f "$PBF" ]; then
    echo "Japan PBF not found: $PBF"
    exit 1
fi

echo "=== Japan extract benchmark ==="
echo "  file: $PBF"
echo "  bbox: $BBOX (Tokyo area)"
echo "  runs: $RUNS (best of)"
echo ""

scripts/build.sh

best_of() {
    local label="$1"
    shift
    local best=""
    local tmpfile
    tmpfile=$(mktemp "$CARGO_TARGET_DIR/.bench_time.XXXXXX")
    for _i in $(seq 1 "$RUNS"); do
        /usr/bin/time -f "%e" "$@" > /dev/null 2> "$tmpfile"
        local t
        t=$(cat "$tmpfile")
        if [ -z "$best" ]; then
            best="$t"
        else
            best=$(python3 -c "print(min(float('$t'), float('$best')))")
        fi
    done
    rm -f "$tmpfile"
    printf "  %-40s %ss\n" "$label" "$best"
}

run_strategy() {
    local strat="$1"
    case "$strat" in
        simple)
            echo "--- extract --simple ---"
            best_of "pbfhogg" "$BIN" extract "$PBF" --simple -b "$BBOX" -o /dev/null
            if command -v osmium &>/dev/null; then
                best_of "osmium" osmium extract "$PBF" -s simple -b "$BBOX" -o "$OSMIUM_OUT" --overwrite
            fi
            echo ""
            ;;
        complete)
            echo "--- extract (complete-ways, default) ---"
            best_of "pbfhogg" "$BIN" extract "$PBF" -b "$BBOX" -o /dev/null
            if command -v osmium &>/dev/null; then
                best_of "osmium" osmium extract "$PBF" -s complete_ways -b "$BBOX" -o "$OSMIUM_OUT" --overwrite
            fi
            echo ""
            ;;
        smart)
            echo "--- extract --smart ---"
            best_of "pbfhogg" "$BIN" extract "$PBF" --smart -b "$BBOX" -o /dev/null
            if command -v osmium &>/dev/null; then
                best_of "osmium" osmium extract "$PBF" -s smart -b "$BBOX" -o "$OSMIUM_OUT" --overwrite
            fi
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

rm -f "$OSMIUM_OUT"
