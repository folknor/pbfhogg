#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
source "$(dirname "$0")/lib.sh"

# ---------------------------------------------------------------------------
# Benchmark merge across I/O backends: buffered, io_uring, io_uring+sqpoll.
# Requires RLIMIT_MEMLOCK >= 16 MB for io_uring registered buffers.
# Usage: bench-merge-uring.sh [base.pbf] [diff.osc.gz] [runs]
# ---------------------------------------------------------------------------

PBF="${1:-data/denmark-20260220-seq4704-with-indexdata.osm.pbf}"
OSC="${2:-data/denmark-20260221-seq4705.osc.gz}"
RUNS="${3:-5}"

if [ ! -f "$PBF" ]; then
    echo "PBF not found: $PBF"
    exit 1
fi
if [ ! -f "$OSC" ]; then
    echo "OSC not found: $OSC"
    exit 1
fi

# Check memlock limit.
MEMLOCK_KB=$(ulimit -l)
if [ "$MEMLOCK_KB" != "unlimited" ] && [ "$MEMLOCK_KB" -lt 16384 ]; then
    echo "ERROR: RLIMIT_MEMLOCK is ${MEMLOCK_KB} KB, need >= 16384 KB (16 MB)."
    echo "  Fix: sudo prlimit --memlock=unlimited --pid=\$\$"
    exit 1
fi

# Build with both features.
cargo build --release --example bench_merge --features linux-io-uring,linux-direct-io

BIN="${CARGO_TARGET_DIR}/release/examples/bench_merge"
MB=$(file_size_mb "$PBF")

echo "=== merge io_uring benchmark ==="
echo "base: $PBF (${MB} MB)"
echo "diff: $OSC"
echo "runs: $RUNS (best of)"
echo ""

# Collect results into an array for the summary table.
declare -a LABELS
declare -a RESULTS

run_bench() {
    local label="$1"
    shift
    echo "--- $label ---"
    # The last line of stderr starting with "elapsed_ms=" has the timing.
    # Capture output; on failure print it so errors are visible.
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
    echo "${label}: ${MS} ms"
    echo ""
    LABELS+=("$label")
    RESULTS+=("$MS")
}

run_bench "buffered+zlib"           --compression zlib
run_bench "buffered+none"           --compression none
run_bench "uring+zlib"              --compression zlib  --io-uring
run_bench "uring+none"              --compression none  --io-uring
run_bench "uring+sqpoll+zlib"       --compression zlib  --io-uring --sqpoll
run_bench "uring+sqpoll+none"       --compression none  --io-uring --sqpoll

echo "==========================================="
echo "SUMMARY (best of $RUNS, $PBF)"
echo "==========================================="
for i in "${!LABELS[@]}"; do
    printf "%-24s %6s ms\n" "${LABELS[$i]}" "${RESULTS[$i]}"
done
