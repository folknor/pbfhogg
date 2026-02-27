#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
source "$(dirname "$0")/lib.sh"

# ---------------------------------------------------------------------------
# Benchmark merge: indexdata+zlib, indexdata+none, and optionally io_uring.
# Output to data/bench-tmp/ (NVMe). Logs to benchmarks/benchmarks-commands.tsv.
#
# Usage:
#   scripts/bench-merge.sh [base.pbf] [diff.osc.gz] [runs] [--uring]
#
# The --uring flag adds io_uring and io_uring+sqpoll variants (requires
# linux-io-uring feature and RLIMIT_MEMLOCK >= 16 MB).
#
# Defaults to Denmark with indexdata.
# ---------------------------------------------------------------------------

URING=false
POSITIONAL=()
for arg in "$@"; do
    if [ "$arg" = "--uring" ]; then
        URING=true
    else
        POSITIONAL+=("$arg")
    fi
done

PBF="${POSITIONAL[0]:-data/denmark-20260220-seq4704-with-indexdata.osm.pbf}"
OSC="${POSITIONAL[1]:-data/denmark-20260221-seq4705.osc.gz}"
RUNS="${POSITIONAL[2]:-3}"

if [ ! -f "$PBF" ]; then
    echo "PBF not found: $PBF"
    exit 1
fi
if [ ! -f "$OSC" ]; then
    echo "OSC not found: $OSC"
    exit 1
fi

NAME="$(basename "${PBF%.osm.pbf}")"
FILE_MB=$(file_size_mb "$PBF")
COMMIT=$(git rev-parse --short HEAD 2>/dev/null || echo "unknown")
DIRTY=""
if ! git diff --quiet HEAD 2>/dev/null; then
    DIRTY="*"
fi
DATE=$(date +%Y-%m-%d)
SUBJECT=$(git log -1 --format=%s 2>/dev/null || echo "-")
HOST=$(hostname)
LOG="benchmarks/benchmarks-commands.tsv"

echo "=== merge benchmark ==="
echo "  base: $PBF ($FILE_MB MB)"
echo "  diff: $OSC"
echo "  runs: $RUNS (best of)"
echo "  commit: ${COMMIT}${DIRTY}"
echo "  uring: $URING"
echo ""

# Build with appropriate features
if [ "$URING" = true ]; then
    MEMLOCK_KB=$(ulimit -l)
    if [ "$MEMLOCK_KB" != "unlimited" ] && [ "$MEMLOCK_KB" -lt 16384 ]; then
        echo "ERROR: RLIMIT_MEMLOCK is ${MEMLOCK_KB} KB, need >= 16384 KB (16 MB)."
        echo "  Fix: sudo prlimit --memlock=unlimited --pid=\$\$"
        exit 1
    fi
    cargo build --release --example bench_merge --features linux-io-uring,linux-direct-io
else
    cargo build --release --example bench_merge
fi

BIN="${CARGO_TARGET_DIR}/release/examples/bench_merge"
echo ""

# TSV header
if [ ! -f "$LOG" ]; then
    mkdir -p benchmarks
    printf "date\thost\tcommit\tsubject\tpbf\ttool\tcommand\telapsed_s\tfile_mb\n" > "$LOG"
fi

# Collect results for summary table
declare -a LABELS
declare -a RESULTS

run_bench() {
    local label="$1"
    local command="$2"
    shift 2

    echo "--- $label ---"
    if ! OUTPUT=$("$BIN" "$PBF" "$OSC" "$RUNS" "$@" 2>&1); then
        echo "$OUTPUT"
        echo "FAILED: $label"
        return 1
    fi
    MS=$(echo "$OUTPUT" | python3 -c "
import sys
for line in sys.stdin:
    if line.startswith('elapsed_ms='):
        print(line.strip().split('=')[1])
")
    SECS=$(python3 -c "print(f'{int($MS) / 1000:.2f}')")
    echo "  ${label}: ${MS} ms (${SECS}s)"
    echo ""

    LABELS+=("$label")
    RESULTS+=("$MS")

    # Log to TSV
    printf "%s\t%s\t%s%s\t%s\t%s\t%s\t%s\t%s\t%s\n" \
        "$DATE" "$HOST" "$COMMIT" "$DIRTY" "$SUBJECT" "$NAME" \
        "pbfhogg" "$command" "$SECS" "$FILE_MB" >> "$LOG"
}

run_bench "buffered+zlib" "merge-buffered-zlib" --compression zlib
run_bench "buffered+none" "merge-buffered-none" --compression none

if [ "$URING" = true ]; then
    run_bench "uring+zlib"         "merge-uring-zlib"         --compression zlib --io-uring
    run_bench "uring+none"         "merge-uring-none"         --compression none --io-uring
    run_bench "uring+sqpoll+zlib"  "merge-uring-sqpoll-zlib"  --compression zlib --io-uring --sqpoll
    run_bench "uring+sqpoll+none"  "merge-uring-sqpoll-none"  --compression none --io-uring --sqpoll
fi

echo "==========================================="
echo "SUMMARY (best of $RUNS, $NAME, $FILE_MB MB)"
echo "==========================================="
for i in "${!LABELS[@]}"; do
    SECS=$(python3 -c "print(f'{int(${RESULTS[$i]}) / 1000:.2f}')")
    printf "  %-24s %6s ms  (%ss)\n" "${LABELS[$i]}" "${RESULTS[$i]}" "$SECS"
done
echo ""
echo "=== Logged to $LOG ==="
