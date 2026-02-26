#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
source "$(dirname "$0")/lib.sh"

PBF="${1:-data/denmark-latest.osm.pbf}"
RUNS="${2:-3}"
COMPRESSION="${3:-none,zlib:6,zstd:3}"
LOG="benchmarks-self-write.tsv"

if [ ! -f "$PBF" ]; then
    echo "PBF not found: $PBF"
    echo "Usage: scripts/bench-self-write.sh [path/to/file.osm.pbf] [runs] [compression]"
    echo "  compression: comma-separated list (default: none,zlib:6,zstd:3)"
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

echo "=== pbfhogg write benchmark ==="
echo "  file: $PBF ($FILE_MB MB)"
echo "  runs: $RUNS (best of)"
echo "  compression: $COMPRESSION"
echo "  commit: ${COMMIT}${DIRTY}"
echo ""

echo "Building (release, zlib-ng)..."
cargo build --release --examples --no-default-features --features zlib-ng 2>&1 | tail -1
BENCH_BIN=$(cargo build --release --example bench_write --no-default-features --features zlib-ng --message-format=json 2>/dev/null \
    | cargo_bin_path)
echo ""

# Create TSV header if needed
if [ ! -f "$LOG" ]; then
    printf "date\tcommit\tsubject\tpbf\tmode\telapsed_ms\tnodes\tways\trelations\tfile_mb\n" > "$LOG"
fi

STDERR_FILE=$(mktemp "$CARGO_TARGET_DIR/.bench_self_write_stderr.XXXXXX")
trap 'rm -f "$STDERR_FILE"' EXIT

"$BENCH_BIN" "$PBF" "$RUNS" --compression "$COMPRESSION" 2> "$STDERR_FILE"

# Parse and record each --- delimited block
in_block=0
mode="" elapsed="" nodes="" ways="" relations="" fmb=""

while IFS= read -r line; do
    if [ "$line" = "---" ]; then
        if [ "$in_block" -eq 1 ] && [ -n "$mode" ]; then
            printf "%s\t%s%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n" \
                "$DATE" "$COMMIT" "$DIRTY" "$SUBJECT" "$NAME" \
                "$mode" "$elapsed" "$nodes" "$ways" "$relations" "$fmb" >> "$LOG"
            printf "  %-14s %6s ms  (%s nodes, %s ways, %s rels)\n" \
                "$mode" "$elapsed" "$nodes" "$ways" "$relations"
        fi
        in_block=1
        mode="" elapsed="" nodes="" ways="" relations="" fmb=""
        continue
    fi
    case "$line" in
        mode=*) mode="${line#mode=}" ;;
        elapsed_ms=*) elapsed="${line#elapsed_ms=}" ;;
        nodes=*) nodes="${line#nodes=}" ;;
        ways=*) ways="${line#ways=}" ;;
        relations=*) relations="${line#relations=}" ;;
        file_mb=*) fmb="${line#file_mb=}" ;;
    esac
done < "$STDERR_FILE"

# Emit last block
if [ "$in_block" -eq 1 ] && [ -n "$mode" ]; then
    printf "%s\t%s%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n" \
        "$DATE" "$COMMIT" "$DIRTY" "$SUBJECT" "$NAME" \
        "$mode" "$elapsed" "$nodes" "$ways" "$relations" "$fmb" >> "$LOG"
    printf "  %-14s %6s ms  (%s nodes, %s ways, %s rels)\n" \
        "$mode" "$elapsed" "$nodes" "$ways" "$relations"
fi

echo ""
echo "=== Logged to $LOG ==="
