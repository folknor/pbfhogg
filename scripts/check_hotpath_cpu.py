#!/usr/bin/env python3
"""Fail unless no build of this workspace can turn on hotpath's CPU profiler.

The CLI promises it starts no helper process (docs/cli/commands.md, "Names
pbfhogg leaves behind"); brokkr's lease cleanup relies on it. The hotpath
dependency starts one, a detached `hotpath-samply`, from its CPU profiler,
which the audited hotpath release (AUDITED) compiles in only under its
`hotpath-cpu` feature (the spawn is `src/lib_on/functions/cpu/autospawn.rs`
in that crate). No pbfhogg
feature names it, but a dependency added later could enable it through
feature unification, which no manifest text shows.

So this reads the resolved graph: `cargo metadata --all-features --locked`.
Features only ever add, so a feature absent with every workspace feature on is
absent from every workspace feature combination, including the
`hotpath` / `hotpath-alloc` builds brokkr makes for `--hotpath` / `--alloc`.
No `--filter-platform`, so every target and dev-dependency counts.

The audit was done against one exact source: the crates.io releases pinned
in AUDITED. A different version or source (a bump, a path or git patch)
fails until someone re-reads that version's feature graph and spawn sites
and updates AUDITED, because a later release could move the spawner behind
another feature. This certifies the checked resolution, with Cargo's source
configuration trusted: a build that changes the lock, the manifests or the
source configuration, or passes dependency features on the command line, is
outside it.

Prints the sentinel last, and only when every assertion held; brokkr's
script_check ignores the exit code.
"""
import json
import subprocess
import sys

SENTINEL = "hotpath-cpu absent"
REGISTRY = "registry+https://github.com/rust-lang/crates.io-index"
AUDITED = {
    "hotpath": "0.30.1",
    "hotpath-macros": "0.30.1",
    "hotpath-drain": "0.30.1",
}
FORBIDDEN = "hotpath-cpu"


def fail(msg):
    print(f"check_hotpath_cpu: {msg}")
    sys.exit(1)


def main():
    proc = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--all-features", "--locked"],
        capture_output=True,
        text=True,
    )
    if proc.returncode != 0:
        fail("audit could not run: cargo metadata failed\n" + proc.stderr)
    try:
        meta = json.loads(proc.stdout)
        packages = {p["id"]: p for p in meta["packages"]}
        nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}
    except (ValueError, KeyError, TypeError) as e:
        fail(f"audit could not run: unexpected cargo metadata shape: {e!r}")

    problems = []
    for name, version in AUDITED.items():
        ids = [pid for pid, p in packages.items() if p.get("name") == name and pid in nodes]
        if not ids:
            problems.append(f"{name}: not in the resolved graph")
            continue
        for pid in ids:
            p = packages[pid]
            if p.get("version") != version or p.get("source") != REGISTRY:
                problems.append(
                    f"{name} {p.get('version')} from {p.get('source')}: audited only "
                    f"{version} from crates.io; re-read its feature graph and spawn "
                    "sites, then update AUDITED"
                )
            features = nodes[pid].get("features")
            if not isinstance(features, list):
                problems.append(f"{name}: resolved node has no feature list")
            elif FORBIDDEN in features:
                problems.append(
                    f"{name} {p.get('version')}: {FORBIDDEN} is enabled; it starts a "
                    "helper process and the CLI promises it starts none"
                )
    if problems:
        fail("\n".join(problems))
    print(SENTINEL)


if __name__ == "__main__":
    main()
