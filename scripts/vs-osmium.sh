#!/usr/bin/env bash
# Run the vs-osmium script bench (benches/compare/vs-osmium.lua, registered
# in brokkr.d/benches.toml) for every row of
# the guide's CLI command table as alternating single-iteration pairs, the
# starting tool reversed from pair to pair. Usage: scripts/vs-osmium.sh [PAIRS]
set -euo pipefail
pairs="${1:-3}"
rows=(sort apply-changes tags-filter-highway tags-filter-restaurant cat-way
      inspect-tags inspect-tags-way getid altw-sparse altw-external)
run() {
    local row="$1" tool="$2" extra=()
    if [ "$row" = apply-changes ]; then extra=(--osc 4705); fi
    brokkr bench --iterations 1 vs-osmium --row "$row" --tool "$tool" "${extra[@]}"
}
for row in "${rows[@]}"; do
    for ((p = 1; p <= pairs; p++)); do
        if ((p % 2)); then run "$row" pbfhogg; run "$row" osmium
        else run "$row" osmium; run "$row" pbfhogg; fi
    done
done
for ((p = 1; p <= pairs; p++)); do run inspect pbfhogg; done
