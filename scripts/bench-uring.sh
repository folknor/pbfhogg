#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

PBF="data/denmark-latest.osm.pbf"
OSC="data/4705.osc.gz"
RUNS=5
BIN="target/release/pbfhogg"

echo "=== Normal pipelined merge (zlib) ==="
for i in $(seq 1 "$RUNS"); do
    rm -f data/bench-normal.osm.pbf
    /usr/bin/time -f "%e" "$BIN" merge "$PBF" "$OSC" -o data/bench-normal.osm.pbf 2>&1
done

echo ""
echo "=== io_uring merge (zlib) ==="
for i in $(seq 1 "$RUNS"); do
    rm -f data/bench-uring.osm.pbf
    /usr/bin/time -f "%e" "$BIN" merge "$PBF" "$OSC" -o data/bench-uring.osm.pbf --io-uring 2>&1
done

echo ""
echo "=== Normal pipelined merge (none) ==="
for i in $(seq 1 "$RUNS"); do
    rm -f data/bench-normal-none.osm.pbf
    /usr/bin/time -f "%e" "$BIN" merge "$PBF" "$OSC" -o data/bench-normal-none.osm.pbf --compression none 2>&1
done

echo ""
echo "=== io_uring merge (none) ==="
for i in $(seq 1 "$RUNS"); do
    rm -f data/bench-uring-none.osm.pbf
    /usr/bin/time -f "%e" "$BIN" merge "$PBF" "$OSC" -o data/bench-uring-none.osm.pbf --io-uring --compression none 2>&1
done

rm -f data/bench-normal.osm.pbf data/bench-uring.osm.pbf data/bench-normal-none.osm.pbf data/bench-uring-none.osm.pbf
