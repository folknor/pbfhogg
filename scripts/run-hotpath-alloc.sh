#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

cargo build --release -p pbfhogg-cli --features hotpath-alloc

HOTPATH_METRICS_SERVER_OFF=true ./target/release/pbfhogg "$@"
