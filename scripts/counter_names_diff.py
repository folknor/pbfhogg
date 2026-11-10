#!/usr/bin/env python3
"""Diff the sidecar counter and marker name sets of two stored brokkr results.

Usage: scripts/counter_names_diff.py <uuid-before> <uuid-after>

The tool for checking that a refactor kept sidecar counter names (the
contract in the crates/lib/src/counters.rs module doc). Reads `brokkr sidecar <uuid>
--counters` and `--markers` (JSONL on stdout). Prints, for counters and then
markers, the names only in the first run, only in the second, and how many are
shared.

Exit 1 when the first run has a counter name the second lacks; exit 2 when
either run's sidecar data cannot be read or holds no counters. Marker
differences are reported but never fail: a missing WAIT_* span can mean no
send blocked in that run, and matching marker names do not prove matching span
boundaries. Equal names do not prove equal units, recording points or
aggregation either; those are judged in the diff.
"""

import json
import subprocess
import sys


class SidecarError(Exception):
    pass


def records(uuid, view):
    proc = subprocess.run(
        ["brokkr", "sidecar", uuid, view],
        capture_output=True,
        text=True,
    )
    if proc.returncode != 0:
        raise SidecarError(f"brokkr sidecar {uuid} {view} exited {proc.returncode}:\n{proc.stderr}")
    names = set()
    for number, line in enumerate(proc.stdout.splitlines(), 1):
        if not line.strip():
            continue
        try:
            record = json.loads(line)
        except json.JSONDecodeError as err:
            raise SidecarError(f"{uuid} {view} line {number}: not JSON: {err}") from err
        name = record.get("name") if isinstance(record, dict) else None
        if not isinstance(name, str) or not name:
            raise SidecarError(f"{uuid} {view} line {number}: no name: {line}")
        names.add(name)
    return names


def report(kind, a_id, b_id, a, b):
    gone = sorted(a - b)
    new = sorted(b - a)
    print(f"{kind}: shared {len(a & b)}")
    print(f"  only in {a_id} ({len(gone)}):")
    for n in gone:
        print(f"    - {n}")
    print(f"  only in {b_id} ({len(new)}):")
    for n in new:
        print(f"    + {n}")
    return gone


def main():
    if len(sys.argv) != 3:
        print(__doc__.strip(), file=sys.stderr)
        return 2
    a_id, b_id = sys.argv[1], sys.argv[2]
    try:
        counters = {u: records(u, "--counters") for u in (a_id, b_id)}
        markers = {u: records(u, "--markers") for u in (a_id, b_id)}
    except SidecarError as err:
        print(err, file=sys.stderr)
        return 2
    for u in (a_id, b_id):
        if not counters[u]:
            print(f"{u}: no counters recorded; nothing to compare", file=sys.stderr)
            return 2
        if not markers[u]:
            print(f"{u}: no markers recorded")
    gone = report("counters", a_id, b_id, counters[a_id], counters[b_id])
    report("markers (informational)", a_id, b_id, markers[a_id], markers[b_id])
    return 1 if gone else 0


if __name__ == "__main__":
    sys.exit(main())
