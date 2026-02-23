#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

PBF="${1:-data/denmark-latest.osm.pbf}"
RUNS="${2:-3}"
LOG="benchmarks.tsv"

if [ ! -f "$PBF" ]; then
    echo "PBF not found: $PBF"
    echo "Usage: scripts/bench.sh [path/to/file.osm.pbf] [runs]"
    exit 1
fi

NAME="$(basename "${PBF%.osm.pbf}")"
FILE_MB=$(( $(stat -c%s "$PBF") / 1000000 ))
COMMIT=$(git rev-parse --short HEAD 2>/dev/null || echo "unknown")
DATE=$(date +%Y-%m-%d)
SUBJECT=$(git log -1 --format=%s 2>/dev/null || echo "-")

echo "=== pbfhogg benchmark ==="
echo "  file: $PBF ($FILE_MB MB)"
echo "  runs: $RUNS (best of)"
echo "  commit: $COMMIT"
echo ""

# Build everything and locate binaries
# Both pbfhogg and osmpbf use zlib-ng for fair comparison
echo "Building pbfhogg with zlib-ng (release)..."
cargo build --release --examples --no-default-features --features zlib-ng 2>&1 | tail -1
PBFHOGG_BIN=$(cargo build --release --example bench_read --no-default-features --features zlib-ng --message-format=json 2>/dev/null \
    | grep '"executable"' | grep -oP '"executable":"\K[^"]+')
MERGE_BIN=$(cargo build --release --example bench_merge --no-default-features --features zlib-ng --message-format=json 2>/dev/null \
    | grep '"executable"' | grep -oP '"executable":"\K[^"]+')
echo "Building osmpbf baseline (release)..."
cargo build --release --manifest-path bench/osmpbf-baseline/Cargo.toml 2>&1 | tail -1
OSMPBF_BIN=$(cargo build --release --manifest-path bench/osmpbf-baseline/Cargo.toml --message-format=json 2>/dev/null \
    | grep '"executable"' | grep -oP '"executable":"\K[^"]+')
echo ""

# Create TSV header if needed
if [ ! -f "$LOG" ]; then
    printf "date\tcommit\tsubject\tpbf\ttool\tmode\telapsed_ms\tnodes\tways\trelations\tfile_mb\n" > "$LOG"
fi

STDERR_FILE=$(mktemp .bench_stderr.XXXXXX)
trap 'rm -f "$STDERR_FILE"' EXIT

parse() { grep -oP "^${1}=\\K.*" "$STDERR_FILE" || echo "-"; }

record_results() {
    # Parse all --- delimited blocks from stderr
    local block_start=0
    local line_num=0

    while IFS= read -r line; do
        line_num=$((line_num + 1))
        if [ "$line" = "---" ]; then
            block_start=$line_num
        fi
    done < "$STDERR_FILE"

    # Process each block
    local in_block=0
    local tool="" mode="" elapsed="" nodes="" ways="" relations="" fmb=""

    while IFS= read -r line; do
        if [ "$line" = "---" ]; then
            # Emit previous block if we had one
            if [ "$in_block" -eq 1 ] && [ -n "$tool" ]; then
                printf "%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n" \
                    "$DATE" "$COMMIT" "$SUBJECT" "$NAME" \
                    "$tool" "$mode" "$elapsed" "$nodes" "$ways" "$relations" "$fmb" >> "$LOG"
                printf "  %-12s %-12s %6s ms\n" "$tool" "$mode" "$elapsed"
            fi
            in_block=1
            tool="" mode="" elapsed="" nodes="" ways="" relations="" fmb=""
            continue
        fi
        case "$line" in
            tool=*) tool="${line#tool=}" ;;
            mode=*) mode="${line#mode=}" ;;
            elapsed_ms=*) elapsed="${line#elapsed_ms=}" ;;
            nodes=*) nodes="${line#nodes=}" ;;
            ways=*) ways="${line#ways=}" ;;
            relations=*) relations="${line#relations=}" ;;
            file_mb=*) fmb="${line#file_mb=}" ;;
        esac
    done < "$STDERR_FILE"

    # Emit last block
    if [ "$in_block" -eq 1 ] && [ -n "$tool" ]; then
        printf "%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n" \
            "$DATE" "$COMMIT" "$SUBJECT" "$NAME" \
            "$tool" "$mode" "$elapsed" "$nodes" "$ways" "$relations" "$fmb" >> "$LOG"
        printf "  %-12s %-12s %6s ms\n" "$tool" "$mode" "$elapsed"
    fi
}

# Run pbfhogg benchmark
echo "--- pbfhogg ---"
"$PBFHOGG_BIN" "$PBF" "$RUNS" 2> "$STDERR_FILE"
record_results
echo ""

# Run osmpbf baseline
echo "--- osmpbf ---"
"$OSMPBF_BIN" "$PBF" "$RUNS" 2> "$STDERR_FILE"
record_results
echo ""

# Run osmium if available
if command -v osmium &>/dev/null; then
    echo "--- osmium ---"
    BEST_MS=999999
    for i in $(seq 1 "$RUNS"); do
        START=$(date +%s%N)
        osmium cat "$PBF" -o /dev/null -f opl --overwrite 2>/dev/null
        END=$(date +%s%N)
        MS=$(( (END - START) / 1000000 ))
        if [ "$MS" -lt "$BEST_MS" ]; then
            BEST_MS=$MS
        fi
    done
    printf "  %-12s %-12s %6s ms\n" "osmium" "cat-opl" "$BEST_MS"
    printf "%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n" \
        "$DATE" "$COMMIT" "$SUBJECT" "$NAME" \
        "osmium" "cat-opl" "$BEST_MS" "-" "-" "-" "$FILE_MB" >> "$LOG"
    echo ""
fi

# Run Planetiler if curl and jq are available
if command -v curl &>/dev/null && command -v jq &>/dev/null; then
    echo "--- planetiler ---"
    scripts/bench-planetiler.sh "$PBF" "$RUNS" 2> "$STDERR_FILE"
    record_results
    echo ""
else
    echo "Skipping Planetiler (curl and jq required)"
    echo ""
fi

# Run merge benchmark if diff file exists
OSC="${OSC:-data/4705.osc.gz}"
if [ -f "$OSC" ]; then
    echo "--- merge ---"
    "$MERGE_BIN" "$PBF" "$OSC" "$RUNS" 2> "$STDERR_FILE"
    record_results
    echo ""

    # Run osmium apply-changes if available
    if command -v osmium &>/dev/null; then
        echo "--- osmium merge ---"
        OSMIUM_OUT=$(mktemp /tmp/osmium-bench-merge.XXXXXX.osm.pbf)
        BEST_MS=999999
        for i in $(seq 1 "$RUNS"); do
            rm -f "$OSMIUM_OUT"
            START=$(date +%s%N)
            osmium apply-changes "$PBF" "$OSC" -o "$OSMIUM_OUT" -O --no-progress 2>/dev/null
            END=$(date +%s%N)
            MS=$(( (END - START) / 1000000 ))
            if [ "$MS" -lt "$BEST_MS" ]; then
                BEST_MS=$MS
            fi
        done
        rm -f "$OSMIUM_OUT"
        printf "  %-12s %-12s %6s ms\n" "osmium" "merge" "$BEST_MS"
        printf "%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n" \
            "$DATE" "$COMMIT" "$SUBJECT" "$NAME" \
            "osmium" "merge" "$BEST_MS" "-" "-" "-" "$FILE_MB" >> "$LOG"
        echo ""
    fi
fi

echo "=== Results recorded to $LOG ==="
tail -15 "$LOG" | column -t -s$'\t'
