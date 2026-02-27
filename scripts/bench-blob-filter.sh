#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
source "$(dirname "$0")/lib.sh"

# Benchmark blob-type skipping: compare indexdata PBF (filter active) vs
# non-indexdata PBF (filter degrades to full decode).

PBF_WITH="${1:-data/denmark-20260220-seq4704-with-indexdata.osm.pbf}"
PBF_WITHOUT="${2:-data/denmark-20260220-seq4704.osm.pbf}"
RUNS="${3:-3}"

echo "Building release..."
scripts/build.sh
BIN="$PBFHOGG"

echo ""
echo "=== cat --type way ==="
echo "--- With indexdata (blob-type skipping active) ---"
for i in $(seq 1 "$RUNS"); do
    /usr/bin/time -f "%e seconds" "$BIN" cat "$PBF_WITH" --type way -o /dev/null 2>&1
done

echo "--- Without indexdata (full decode, filter at element level) ---"
for i in $(seq 1 "$RUNS"); do
    /usr/bin/time -f "%e seconds" "$BIN" cat "$PBF_WITHOUT" --type way -o /dev/null 2>&1
done

echo ""
echo "=== cat --type relation ==="
echo "--- With indexdata (blob-type skipping active) ---"
for i in $(seq 1 "$RUNS"); do
    /usr/bin/time -f "%e seconds" "$BIN" cat "$PBF_WITH" --type relation -o /dev/null 2>&1
done

echo "--- Without indexdata (full decode, filter at element level) ---"
for i in $(seq 1 "$RUNS"); do
    /usr/bin/time -f "%e seconds" "$BIN" cat "$PBF_WITHOUT" --type relation -o /dev/null 2>&1
done

echo ""
echo "=== tags-count --type way ==="
echo "--- With indexdata ---"
for i in $(seq 1 "$RUNS"); do
    /usr/bin/time -f "%e seconds" "$BIN" tags-count "$PBF_WITH" --type way --min-count 999999999 2>&1
done

echo "--- Without indexdata ---"
for i in $(seq 1 "$RUNS"); do
    /usr/bin/time -f "%e seconds" "$BIN" tags-count "$PBF_WITHOUT" --type way --min-count 999999999 2>&1
done

echo ""
echo "=== node-stats ==="
echo "--- With indexdata (skip way+relation blobs) ---"
for i in $(seq 1 "$RUNS"); do
    /usr/bin/time -f "%e seconds" "$BIN" node-stats "$PBF_WITH" 2>&1 > /dev/null
done

echo "--- Without indexdata ---"
for i in $(seq 1 "$RUNS"); do
    /usr/bin/time -f "%e seconds" "$BIN" node-stats "$PBF_WITHOUT" 2>&1 > /dev/null
done
