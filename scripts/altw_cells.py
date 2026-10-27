"""Tabulate altw external phase durations and worker counters for brokkr cells.

Usage: python3 scripts/altw_cells.py UUID [UUID ...]
"""
import json
import subprocess
import sys

PHASES = ["EXTJOIN_S1_PASS_A", "EXTJOIN_STAGE2", "EXTJOIN_STREAMING"]
COUNTERS = [
    "s1a_scan_way_refs_ms",
    "s1a_id_shard_write_ms",
    "s2_node_extract_ns",
    "s2_node_walk_ns",
    "s2_node_pread_ms",
    "s3_read_ms",
    "s4_nonway_assemble_ms",
    "s4_way_reframe_ms",
    "s4_coord_payload_pread_ms",
    "s4_decompress_ms",
    "s4_pread_ms",
]


def sidecar(uuid, *args):
    out = subprocess.run(
        ["brokkr", "sidecar", uuid, *args], capture_output=True, text=True, check=True
    ).stdout
    return [json.loads(line) for line in out.splitlines() if line.startswith("{")]


def main():
    rows = []
    for uuid in sys.argv[1:]:
        durations = {
            r["name"]: r["duration_ms"]
            for r in sidecar(uuid, "--durations")
            if r.get("type") == "phase" and r["name"] in PHASES
        }
        counters = {}
        for r in sidecar(uuid, "--counters"):
            name = r.get("name")
            if name in COUNTERS:
                counters[name] = r.get("value")
        # Every segment, repeated names (the WAIT_* pairs) included: keying
        # them by name dropped all but the last of each.
        write_kb = sum(
            r.get("disk_write_kb", 0) or 0
            for r in sidecar(uuid)
            if r.get("type") == "phase"
        )
        rows.append((uuid, durations, counters, write_kb))
    header = ["cell"] + PHASES + COUNTERS + ["disk_write_GB"]
    print("\t".join(header))
    for uuid, durations, counters, write_kb in rows:
        line = [uuid]
        line += [f"{durations.get(p, 0) / 1000:.1f}" for p in PHASES]
        for c in COUNTERS:
            v = counters.get(c)
            if v is None:
                line.append("-")
            elif c.endswith("_ns"):
                line.append(f"{v / 1e9:.1f}")
            else:
                line.append(f"{v / 1e3:.1f}")
        line.append(f"{write_kb / 1e6:.1f}")
        print("\t".join(line))


main()
