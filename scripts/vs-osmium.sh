#!/usr/bin/env bash
# Alternate single-iteration arms from the Lua row registry.
# Usage: scripts/vs-osmium.sh PAIRS DATASET OSC
# OSC is an explicit key in DATASET's OSC table, resolved by brokkr.
set -euo pipefail
if [[ $# != 3 || ! $1 =~ ^[1-9][0-9]{0,8}$ ]]; then
    echo "usage: scripts/vs-osmium.sh PAIRS DATASET OSC (PAIRS must be 1..999999999)" >&2
    exit 2
fi
pairs="$1"
dataset="$2"
osc="$3"
[[ -n "$dataset" && -n "$osc" ]] || { echo "DATASET and OSC must be nonempty" >&2; exit 2; }
root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
# Top-level row keys are the registry. Keep their spelling in Lua as ["name"].
mapfile -t rows < <(sed -n 's/^        \["\([a-z-]*\)"\] = {$/\1/p' "$root/benches/compare/vs-osmium.lua")
((${#rows[@]} > 0)) || { echo "empty comparison registry" >&2; exit 2; }
run() {
    local row="$1" tool="$2" extra=()
    if [[ "$row" == apply-changes ]]; then extra=(--osc "$osc"); fi
    brokkr bench --iterations 1 vs-osmium --dataset "$dataset" --row "$row" --tool "$tool" "${extra[@]}"
}
for row in "${rows[@]}"; do
    for ((p = 1; p <= pairs; p++)); do
        if [[ "$row" == inspect ]]; then run "$row" pbfhogg
        elif ((p % 2)); then run "$row" pbfhogg; run "$row" osmium
        else run "$row" osmium; run "$row" pbfhogg; fi
    done
done
