#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
source "$(dirname "$0")/lib.sh"

PBF="data/denmark-latest.osm.pbf"
OSC="data/4705.osc.gz"
RUNS=5
BIN="$PBFHOGG"

if [[ ! -x /usr/bin/time ]]; then
    echo "ERROR: /usr/bin/time not found (sudo apt install time)"
    exit 1
fi

scripts/build.sh

echo "=== Normal pipelined merge (zlib) ==="
for i in $(seq 1 "$RUNS"); do
    rm -f "$CARGO_TARGET_DIR/bench-normal.osm.pbf"
    /usr/bin/time -f "%e" "$BIN" merge "$PBF" "$OSC" -o "$CARGO_TARGET_DIR/bench-normal.osm.pbf" 2>&1
done

echo ""
echo "=== io_uring merge (zlib) ==="
for i in $(seq 1 "$RUNS"); do
    rm -f "$CARGO_TARGET_DIR/bench-uring.osm.pbf"
    /usr/bin/time -f "%e" "$BIN" merge "$PBF" "$OSC" -o "$CARGO_TARGET_DIR/bench-uring.osm.pbf" --io-uring 2>&1
done

echo ""
echo "=== Normal pipelined merge (none) ==="
for i in $(seq 1 "$RUNS"); do
    rm -f "$CARGO_TARGET_DIR/bench-normal-none.osm.pbf"
    /usr/bin/time -f "%e" "$BIN" merge "$PBF" "$OSC" -o "$CARGO_TARGET_DIR/bench-normal-none.osm.pbf" --compression none 2>&1
done

echo ""
echo "=== io_uring merge (none) ==="
for i in $(seq 1 "$RUNS"); do
    rm -f "$CARGO_TARGET_DIR/bench-uring-none.osm.pbf"
    /usr/bin/time -f "%e" "$BIN" merge "$PBF" "$OSC" -o "$CARGO_TARGET_DIR/bench-uring-none.osm.pbf" --io-uring --compression none 2>&1
done

rm -f "$CARGO_TARGET_DIR/bench-normal.osm.pbf" "$CARGO_TARGET_DIR/bench-uring.osm.pbf" "$CARGO_TARGET_DIR/bench-normal-none.osm.pbf" "$CARGO_TARGET_DIR/bench-uring-none.osm.pbf"
