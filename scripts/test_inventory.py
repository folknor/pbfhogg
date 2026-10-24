#!/usr/bin/env python3
"""Summarize a brokkr check accounting plan into a per-lane test inventory.

The plan is `.brokkr/accounting/<run id>/plan.json`, which every
`brokkr check` run that runs tests writes before its first test (the newest
few run records are kept). `brokkr check --gate` (every sweep, unfiltered)
gives the whole-suite plan; `brokkr check --profile <name>` the profile's.
Take one dump from a tree before a test move and one after, then compare.

Limit: tests are matched by libtest name only, and the package is ignored on
purpose so a moved test is not a loss. The same blindness means a lost test
can be masked by a same-named test in the other package: a name that still
runs in the lane, in either package, counts as present.

Usage:
  test_inventory.py dump <plan.json> <out.txt>
      Write one line per planned execution: `lane<TAB>where<TAB>name`, where
      `where` is package/kind/target and `name` the libtest name, so two
      dumps can be compared. Package and target are kept apart from the name
      because a test that moves between packages keeps its libtest name.
  test_inventory.py compare <before.txt> <after.txt> [<renames.txt>]
      Per lane: counts before and after, and every libtest name that ran in a
      lane before and does not run in that lane after (the package is
      ignored, so a test moved from `pbfhogg` to `pbfhogg-cli` is not a
      loss). `renames.txt` holds tab-separated lines applied to the BEFORE
      side first:
        lane<TAB>OLD<TAB>NEW      lane OLD's tests are expected in lane NEW
        name<TAB>OLD<TAB>NEW      a test renamed from OLD to NEW
        prefix<TAB>LANE<TAB>P     in LANE, every name gains prefix P
      Exits 1 if anything was lost.
"""
import collections
import json
import os
import sys


def dump(plan_path, out_path):
    plan = json.load(open(plan_path))
    lines = []
    for lane in plan["lanes"]:
        label = lane["label"]
        inventory = lane["inventory"]
        if not isinstance(inventory, dict):
            # A doc-only lane: doctests are not enumerable.
            print(f"{label}: {inventory} (not enumerated)")
            continue
        for execution in inventory["prepared"]["executions"]:
            unit = execution["unit"]
            where = f"{unit['package']}/{unit['kind']}/{unit['target']}"
            for name in execution["tests"]:
                lines.append(f"{label}\t{where}\t{name}")
    lines.sort()
    os.makedirs(os.path.dirname(os.path.abspath(out_path)), exist_ok=True)
    with open(out_path, "w") as out:
        out.write("\n".join(lines) + "\n")
    counts = collections.Counter(line.split("\t")[0] for line in lines)
    for label, count in sorted(counts.items()):
        print(f"{label}: {count}")
    print(f"total: {len(lines)}")


def load(path, renames=None):
    renames = renames or {"lane": {}, "name": {}, "prefix": {}}
    by_lane = collections.defaultdict(collections.Counter)
    for line in open(path):
        line = line.rstrip("\n")
        if not line:
            continue
        label, _where, name = line.split("\t")
        prefix = renames["prefix"].get(label)
        if prefix:
            name = prefix + name
        name = renames["name"].get(name, name)
        label = renames["lane"].get(label, label)
        by_lane[label][name] += 1
    return by_lane


def load_renames(path):
    renames = {"lane": {}, "name": {}, "prefix": {}}
    for line in open(path):
        line = line.rstrip("\n")
        if not line or line.startswith("#"):
            continue
        kind, old, new = line.split("\t")
        renames[kind][old] = new
    return renames


def compare(before_path, after_path, renames_path=None):
    renames = load_renames(renames_path) if renames_path else None
    before = load(before_path, renames)
    after = load(after_path)
    lost_any = False
    for label in sorted(set(before) | set(after)):
        b = before.get(label, collections.Counter())
        a = after.get(label, collections.Counter())
        print(f"{label}: before {sum(b.values())}, after {sum(a.values())}")
        lost = sorted((b - a).elements())
        gained = sorted((a - b).elements())
        for name in lost:
            lost_any = True
            print(f"  lost: {name}")
        if gained:
            print(f"  gained: {len(gained)}")
            for name in gained:
                print(f"    + {name}")
    if lost_any:
        sys.exit(1)


if __name__ == "__main__":
    if len(sys.argv) == 4 and sys.argv[1] == "dump":
        dump(sys.argv[2], sys.argv[3])
    elif len(sys.argv) in (4, 5) and sys.argv[1] == "compare":
        compare(*sys.argv[2:])
    else:
        print(__doc__, file=sys.stderr)
        sys.exit(2)
