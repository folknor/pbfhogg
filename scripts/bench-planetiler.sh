#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

PBF="${1:-data/denmark-latest.osm.pbf}"
RUNS="${2:-3}"
BENCH_SRC="bench/planetiler-baseline/BenchPbfRead.java"

if [ ! -f "$PBF" ]; then
    echo "PBF not found: $PBF"
    echo "Usage: scripts/bench-planetiler.sh [path/to/file.osm.pbf] [runs]"
    exit 1
fi

for cmd in jq curl; do
    if ! command -v "$cmd" &>/dev/null; then
        echo "Error: $cmd is required but not found"
        exit 1
    fi
done

FILE_MB=$(( $(stat -Lc%s "$PBF") / 1000000 ))

# --- Temurin JRE setup ---
# We need a JDK (not JRE) for javac to compile our benchmark
JDK_MAJOR=25
JDK_DIR="data/jdk"
JDK_VERSION_FILE="data/.jdk-version"
JAVA="$JDK_DIR/bin/java"
JAVAC="$JDK_DIR/bin/javac"

ensure_jdk() {
    echo "Checking Temurin JDK ${JDK_MAJOR}..."
    local api_url="https://api.adoptium.net/v3/assets/latest/${JDK_MAJOR}/hotspot?architecture=x64&image_type=jdk&os=linux&vendor=eclipse"
    local api_json
    api_json=$(curl -sfL "$api_url") || { echo "Error: failed to query Adoptium API"; exit 1; }

    local release_name download_url
    release_name=$(echo "$api_json" | jq -r '.[0].release_name')
    download_url=$(echo "$api_json" | jq -r '.[0].binary.package.link')

    if [ -f "$JDK_VERSION_FILE" ] && [ "$(cat "$JDK_VERSION_FILE")" = "$release_name" ] && [ -x "$JAVA" ]; then
        echo "  JDK up to date: $release_name"
        return
    fi

    echo "  Downloading Temurin JDK $release_name..."
    local tarball="data/jdk-download.tar.gz"
    curl -fsSL -o "$tarball" "$download_url"
    rm -rf "$JDK_DIR"
    mkdir -p "$JDK_DIR"
    tar xzf "$tarball" -C "$JDK_DIR" --strip-components=1
    rm -f "$tarball"
    echo "$release_name" > "$JDK_VERSION_FILE"
    echo "  Installed: $("$JAVA" -version 2>&1 | head -1)"
}

# --- Planetiler JAR setup ---
PLANETILER_JAR="data/planetiler.jar"
PLANETILER_VERSION_FILE="data/.planetiler-version"

ensure_planetiler() {
    echo "Checking Planetiler..."
    local api_json
    api_json=$(curl -sfL "https://api.github.com/repos/onthegomap/planetiler/releases/latest") || {
        echo "Error: failed to query GitHub API"; exit 1;
    }

    local tag_name download_url
    tag_name=$(echo "$api_json" | jq -r '.tag_name')
    download_url=$(echo "$api_json" | jq -r '.assets[] | select(.name == "planetiler.jar") | .browser_download_url')

    if [ -f "$PLANETILER_VERSION_FILE" ] && [ "$(cat "$PLANETILER_VERSION_FILE")" = "$tag_name" ] && [ -f "$PLANETILER_JAR" ]; then
        echo "  Planetiler up to date: $tag_name"
        return
    fi

    echo "  Downloading Planetiler $tag_name..."
    curl -fsSL -o "$PLANETILER_JAR" "$download_url"
    echo "$tag_name" > "$PLANETILER_VERSION_FILE"
    echo "  Installed: $tag_name ($(du -h "$PLANETILER_JAR" | cut -f1))"
}

# --- Compile benchmark ---
BENCH_CLASS_DIR="data/planetiler-bench-classes"

compile_bench() {
    # Recompile if source is newer than class file
    local class_file="$BENCH_CLASS_DIR/BenchPbfRead.class"
    if [ -f "$class_file" ] && [ "$class_file" -nt "$BENCH_SRC" ] && [ "$class_file" -nt "$PLANETILER_JAR" ]; then
        return
    fi
    echo "Compiling benchmark..."
    mkdir -p "$BENCH_CLASS_DIR"
    "$JAVAC" -proc:none -cp "$PLANETILER_JAR" -d "$BENCH_CLASS_DIR" "$BENCH_SRC"
}

# --- Main ---
mkdir -p data
ensure_jdk
ensure_planetiler
compile_bench
echo ""

HEAP_MB=$(( FILE_MB * 2 ))
if [ "$HEAP_MB" -lt 2048 ]; then
    HEAP_MB=2048
fi

echo "=== Planetiler PBF read benchmark ==="
echo "  file: $PBF ($FILE_MB MB)"
echo "  runs: $RUNS (best of)"
echo "  heap: ${HEAP_MB}m"
echo ""

# Run benchmark, capturing stderr for summary while passing it through.
# BenchPbfRead outputs --- delimited key=value blocks to stderr.
BENCH_STDERR=$(mktemp)
trap 'rm -f "$BENCH_STDERR"' EXIT

"$JAVA" "-Xmx${HEAP_MB}m" \
    -cp "$PLANETILER_JAR:$BENCH_CLASS_DIR" \
    BenchPbfRead "$PBF" "$RUNS" 2> >(tee "$BENCH_STDERR" >&2)

# Wait for tee process substitution to finish
sleep 0.1

# Print summary to stdout
while IFS= read -r line; do
    case "$line" in
        tool=*) tool="${line#tool=}" ;;
        mode=*) mode="${line#mode=}" ;;
        elapsed_ms=*) elapsed="${line#elapsed_ms=}"
            printf "  %-12s %-12s %6s ms\n" "$tool" "$mode" "$elapsed" ;;
    esac
done < "$BENCH_STDERR"
