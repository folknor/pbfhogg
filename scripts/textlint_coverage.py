#!/usr/bin/env python3
"""Record or check the set of files every [[textlint]] rule admits.

A textlint glob that selects nothing is not a failure in an ordinary
`brokkr check`, so a layout change can leave a rule scanning nothing and
still go green. This script pins what each rule in brokkr.d/lints.toml
scans, so the move of every member under crates/ can prove every rule still
covers the same files.

    textlint_coverage.py record OUT        write the manifest
    textlint_coverage.py check OUT         compare the tree to the manifest
    textlint_coverage.py check OUT --moved compare, mapping the recorded
                                           paths through scripts/move_map.py

A rule's admitted set is the tracked and untracked-not-ignored files its
`paths` select, minus those its `exclude` matches (brokkr's selector
semantics: `**` crosses directory separators, `*` stays within one
component). Its `must_scan` list is recorded as written. `check` fails on
any difference, rule by rule, and on any glob or must_scan entry that
selects no file; `record` refuses the same dead selectors, so the pinned
manifest never starts out blind.
"""
from pathlib import Path
import json
import re
import subprocess
import sys
import tomllib

sys.path.insert(0, str(Path(__file__).resolve().parent))
from move_map import map_path  # noqa: E402

root = Path(__file__).resolve().parent.parent


def glob_regex(glob):
    out, i = [], 0
    while i < len(glob):
        if glob.startswith("**/", i):
            out.append("(?:.*/)?")
            i += 3
        elif glob.startswith("**", i):
            out.append(".*")
            i += 2
        elif glob[i] == "*":
            out.append("[^/]*")
            i += 1
        elif glob[i] == "?":
            out.append("[^/]")
            i += 1
        else:
            out.append(re.escape(glob[i]))
            i += 1
    return re.compile("".join(out) + r"\Z")


def snapshot():
    listed = subprocess.run(
        ["git", "ls-files", "-z", "--cached", "--others", "--exclude-standard"],
        cwd=root, check=True, capture_output=True,
    ).stdout.decode()
    return sorted(p for p in set(listed.split("\0")) if p and (root / p).is_file())


def coverage(files):
    """(manifest, dead selectors) for the current tree."""
    config = tomllib.loads((root / "brokkr.d/lints.toml").read_text())
    manifest, dead = {}, []
    for rule in config.get("textlint", []):
        name = rule["name"]
        selected = set()
        for glob in rule["paths"]:
            hits = {f for f in files if glob_regex(glob).match(f)}
            if not hits:
                dead.append(f"{name}: paths glob {glob!r} selects no file")
            selected |= hits
        admitted = set(selected)
        for glob in rule.get("exclude", []):
            hits = {f for f in selected if glob_regex(glob).match(f)}
            if not hits:
                dead.append(f"{name}: exclude {glob!r} excuses no selected file")
            admitted -= hits
        for path in rule.get("must_scan", []):
            if path not in admitted:
                dead.append(f"{name}: must_scan {path!r} is not an admitted file")
        manifest[name] = {
            "admitted": sorted(admitted),
            "must_scan": sorted(rule.get("must_scan", [])),
        }
    return manifest, dead


def main(argv):
    if len(argv) < 2 or argv[0] not in ("record", "check"):
        print(__doc__, file=sys.stderr)
        return 2
    mode, out = argv[0], Path(argv[1])
    if not out.is_absolute():
        out = Path.cwd() / out
    manifest, dead = coverage(snapshot())
    errors = list(dead)
    if mode == "record":
        if errors:
            print("\n".join(errors), file=sys.stderr)
            return 1
        out.write_text(json.dumps(manifest, indent=1, sort_keys=True) + "\n")
        total = sum(len(r["admitted"]) for r in manifest.values())
        print(f"recorded {len(manifest)} rules, {total} admitted paths, to {out}")
        print("textlint coverage recorded")
        return 0
    recorded = json.loads(out.read_text())
    if "--moved" in argv[2:]:
        recorded = {
            name: {key: sorted(map_path(p) for p in paths) for key, paths in rule.items()}
            for name, rule in recorded.items()
        }
    for name in sorted(set(recorded) | set(manifest)):
        if name not in manifest:
            errors.append(f"{name}: recorded rule no longer exists")
            continue
        if name not in recorded:
            errors.append(f"{name}: rule not in the recorded manifest")
            continue
        for key in ("admitted", "must_scan"):
            was, now = set(recorded[name][key]), set(manifest[name][key])
            for p in sorted(was - now):
                errors.append(f"{name}: {key} lost {p}")
            for p in sorted(now - was):
                errors.append(f"{name}: {key} gained {p}")
    if errors:
        print("\n".join(errors), file=sys.stderr)
        return 1
    print("textlint coverage unchanged")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
