#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
source "$(dirname "$0")/lib.sh"

# ---------------------------------------------------------------------------
# Benchmark pbfhogg CLI commands vs osmium. Runs best-of-N wall-clock times.
#
# Usage:
#   scripts/bench-commands.sh <command> [pbf] [runs]
#   scripts/bench-commands.sh all       [pbf] [runs]
#
# Commands:
#   cat-way, cat-relation, tags-count, tags-count-way, tags-filter-way,
#   tags-filter-amenity, getid, removeid, add-locations-to-ways, node-stats,
#   all (runs everything)
#
# Examples:
#   scripts/bench-commands.sh cat-way
#   scripts/bench-commands.sh tags-count data/japan.osm.pbf 5
#   scripts/bench-commands.sh all data/denmark-20260220-seq4704.osm.pbf 3
# ---------------------------------------------------------------------------

CMD="${1:-all}"
PBF="${2:-data/denmark-20260220-seq4704.osm.pbf}"
RUNS="${3:-3}"

if [ ! -f "$PBF" ]; then
    echo "PBF not found: $PBF"
    echo "Usage: scripts/bench-commands.sh <command> [pbf] [runs]"
    exit 1
fi

PBF_IDX="${PBF%.osm.pbf}-with-indexdata.osm.pbf"
FILE_MB=$(file_size_mb "$PBF")
COMMIT=$(git rev-parse --short HEAD 2>/dev/null || echo "unknown")
HAS_IDX="no"
if [ -f "$PBF_IDX" ]; then
    HAS_IDX="yes"
fi
HAS_OSMIUM="no"
if command -v osmium &>/dev/null; then
    HAS_OSMIUM="yes"
fi
# osmium can't write to /dev/null (format detection), use a temp file
OSMIUM_OUT="$CARGO_TARGET_DIR/bench-commands-osmium.osm.pbf"

echo "=== bench-commands: $CMD ==="
echo "  file: $PBF ($FILE_MB MB)"
echo "  indexdata: $HAS_IDX"
echo "  runs: $RUNS (best of)"
echo "  commit: $COMMIT"
echo ""

scripts/build.sh
BIN="$PBFHOGG"
echo ""

# ---------------------------------------------------------------------------
# Timing helper: run a command RUNS times, print best wall-clock time.
# Stderr from the command is discarded; timing comes from /usr/bin/time.
# ---------------------------------------------------------------------------
best_of() {
    local label="$1"
    shift
    local best=""
    local tmpfile
    tmpfile=$(mktemp "$CARGO_TARGET_DIR/.bench_time.XXXXXX")

    for _i in $(seq 1 "$RUNS"); do
        /usr/bin/time -f "%e" "$@" >"$tmpfile" 2>&1 || true
        local elapsed
        elapsed=$(tail -1 "$tmpfile")
        if [ -z "$best" ]; then
            best="$elapsed"
        else
            best=$(python3 -c "print(min(float('$elapsed'), float('$best')))")
        fi
    done

    rm -f "$tmpfile"
    printf "  %-45s %ss\n" "$label" "$best"
}

# ---------------------------------------------------------------------------
# Individual command benchmarks
# ---------------------------------------------------------------------------

bench_cat_way() {
    echo "=== cat --type way ==="
    if [ "$HAS_IDX" = "yes" ]; then
        best_of "pbfhogg (indexdata)" "$BIN" cat "$PBF_IDX" --type way -o /dev/null
    fi
    best_of "pbfhogg" "$BIN" cat "$PBF" --type way -o /dev/null
    if [ "$HAS_OSMIUM" = "yes" ]; then
        best_of "osmium" osmium cat "$PBF" -t way -o "$OSMIUM_OUT" --overwrite
    fi
    echo ""
}

bench_cat_relation() {
    echo "=== cat --type relation ==="
    if [ "$HAS_IDX" = "yes" ]; then
        best_of "pbfhogg (indexdata)" "$BIN" cat "$PBF_IDX" --type relation -o /dev/null
    fi
    best_of "pbfhogg" "$BIN" cat "$PBF" --type relation -o /dev/null
    if [ "$HAS_OSMIUM" = "yes" ]; then
        best_of "osmium" osmium cat "$PBF" -t relation -o "$OSMIUM_OUT" --overwrite
    fi
    echo ""
}

bench_tags_count() {
    echo "=== tags-count (all) ==="
    best_of "pbfhogg" "$BIN" tags-count "$PBF" --min-count 999999999
    if [ "$HAS_OSMIUM" = "yes" ]; then
        best_of "osmium" osmium tags-count "$PBF" --min-count 999999999
    fi
    echo ""
}

bench_tags_count_way() {
    echo "=== tags-count --type way ==="
    if [ "$HAS_IDX" = "yes" ]; then
        best_of "pbfhogg (indexdata)" "$BIN" tags-count "$PBF_IDX" --type way --min-count 999999999
    fi
    best_of "pbfhogg" "$BIN" tags-count "$PBF" --type way --min-count 999999999
    if [ "$HAS_OSMIUM" = "yes" ]; then
        best_of "osmium" osmium tags-count "$PBF" -t way --min-count 999999999
    fi
    echo ""
}

bench_tags_filter_way() {
    echo "=== tags-filter w/highway=primary -R ==="
    best_of "pbfhogg" "$BIN" tags-filter "$PBF" -R w/highway=primary -o /dev/null
    if [ "$HAS_OSMIUM" = "yes" ]; then
        best_of "osmium" osmium tags-filter "$PBF" w/highway=primary -R -o "$OSMIUM_OUT" --overwrite
    fi
    echo ""
}

bench_tags_filter_amenity() {
    echo "=== tags-filter amenity=restaurant -R ==="
    best_of "pbfhogg" "$BIN" tags-filter "$PBF" -R amenity=restaurant -o /dev/null
    if [ "$HAS_OSMIUM" = "yes" ]; then
        best_of "osmium" osmium tags-filter "$PBF" amenity=restaurant -R -o "$OSMIUM_OUT" --overwrite
    fi
    echo ""
}

bench_getid() {
    echo "=== getid (9 elements) ==="
    best_of "pbfhogg" "$BIN" getid "$PBF" n115722 n115723 n115724 w2080 w2081 w2082 r174 r213 r339 -o /dev/null
    if [ "$HAS_OSMIUM" = "yes" ]; then
        best_of "osmium" osmium getid "$PBF" n115722 n115723 n115724 w2080 w2081 w2082 r174 r213 r339 -o "$OSMIUM_OUT" --overwrite
    fi
    echo ""
}

bench_removeid() {
    echo "=== removeid (9 elements removed) ==="
    best_of "pbfhogg" "$BIN" removeid "$PBF" n115722 n115723 n115724 w2080 w2081 w2082 r174 r213 r339 -o /dev/null
    echo ""
}

bench_add_locations_to_ways() {
    echo "=== add-locations-to-ways ==="
    best_of "pbfhogg" "$BIN" add-locations-to-ways "$PBF" -o /dev/null
    if [ "$HAS_OSMIUM" = "yes" ]; then
        best_of "osmium" osmium add-locations-to-ways "$PBF" -o "$OSMIUM_OUT" --overwrite
    fi
    echo ""
}

bench_node_stats() {
    echo "=== node-stats ==="
    if [ "$HAS_IDX" = "yes" ]; then
        best_of "pbfhogg (indexdata)" "$BIN" node-stats "$PBF_IDX"
    fi
    best_of "pbfhogg" "$BIN" node-stats "$PBF"
    echo ""
}

# ---------------------------------------------------------------------------
# Dispatch
# ---------------------------------------------------------------------------

case "$CMD" in
    cat-way)                bench_cat_way ;;
    cat-relation)           bench_cat_relation ;;
    tags-count)             bench_tags_count ;;
    tags-count-way)         bench_tags_count_way ;;
    tags-filter-way)        bench_tags_filter_way ;;
    tags-filter-amenity)    bench_tags_filter_amenity ;;
    getid)                  bench_getid ;;
    removeid)               bench_removeid ;;
    add-locations-to-ways)  bench_add_locations_to_ways ;;
    node-stats)             bench_node_stats ;;
    all)
        bench_cat_way
        bench_cat_relation
        bench_tags_count
        bench_tags_count_way
        bench_tags_filter_way
        bench_tags_filter_amenity
        bench_getid
        bench_removeid
        bench_add_locations_to_ways
        bench_node_stats
        ;;
    *)
        echo "Unknown command: $CMD"
        echo ""
        echo "Available commands:"
        echo "  cat-way, cat-relation, tags-count, tags-count-way,"
        echo "  tags-filter-way, tags-filter-amenity, getid, removeid,"
        echo "  add-locations-to-ways, node-stats, all"
        exit 1
        ;;
esac

rm -f "$OSMIUM_OUT"
echo "Done."
