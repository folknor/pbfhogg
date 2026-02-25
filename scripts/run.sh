#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

cargo run --release -p pbfhogg-cli -- "$@"
