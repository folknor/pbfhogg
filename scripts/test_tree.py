#!/usr/bin/env python3
"""Test-run hygiene for `brokkr check`: clear test scratch, then prove the
test phase left nothing behind in the checkout.

brokkr runs this as two `[[script_check]]` entries (brokkr.d/lints.toml):

  clear-and-snapshot  (pre-test)  empties data/test-scratch and records a
                                  manifest of the checkout.
  verify              (post-test) compares the checkout with that manifest
                                  and requires data/test-scratch to be empty.

The test runner clears nothing and checks nothing itself, so pbfhogg owns
both. Clearing at the start of each checked run bounds residue: a failing
test's kept tree survives until the next run's pre-test stage, which is the
inspection window.

The manifest is stat-based: per entry, its type and mode, a symlink's
target, and for regular files and symlinks size, mtime_ns and ctime_ns. No
content digest: data/ holds planet-sized inputs. It therefore detects
changes that remain, not writes that were undone, and a same-size rewrite
landing on an identical timestamp would pass. Symlinks are recorded, never
followed, so a write through one to a target outside the checkout is unseen.

Excluded contents, by exact checkout-relative path (their existence and
type are still recorded):
  .git                 git's own state
  target               build output
  .brokkr              brokkr writes logs and stores during the run
  data/test-scratch    owned by the clear and the emptiness check here
  scripts/__pycache__  bytecode from the other python script checks
Excluded entirely: Cargo.lock at the root (builds may rewrite it) and this
script's own snapshot files.

Not covered: `brokkr test` (it runs no script checks), and a run whose test
phase or coverage audit failed (brokkr skips post-test checks). Writes made
by the prepare builds between the two stages are attributed to the test
interval, and an edit by hand during the run fails the check.

The sentinel is printed only after the step fully succeeded; brokkr ignores
the exit code.
"""

import json
import os
import stat
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SCRATCH = "data/test-scratch"
SNAPSHOT = "data/test-tree-snapshot.json"
SNAPSHOT_TMP = SNAPSHOT + ".tmp"

PRUNED = {".git", "target", ".brokkr", SCRATCH, "scripts/__pycache__"}
SKIPPED = {"Cargo.lock", SNAPSHOT, SNAPSHOT_TMP}


def fail(message):
    print(message)
    print("test tree check FAILED")
    sys.exit(1)


def kind(mode):
    if stat.S_ISLNK(mode):
        return "link"
    if stat.S_ISDIR(mode):
        return "dir"
    if stat.S_ISREG(mode):
        return "file"
    return "other"


def record(rel, st):
    entry = {"type": kind(st.st_mode)}
    if rel in PRUNED:
        # A boundary: its existence and type are protected, its contents
        # and mode belong to its owner.
        if entry["type"] == "link":
            entry["target"] = os.readlink(os.path.join(ROOT, rel))
        return entry
    entry["mode"] = stat.S_IMODE(st.st_mode)
    if entry["type"] == "link":
        entry["target"] = os.readlink(os.path.join(ROOT, rel))
    if entry["type"] != "dir":
        entry["size"] = st.st_size
        entry["mtime_ns"] = st.st_mtime_ns
        entry["ctime_ns"] = st.st_ctime_ns
    return entry


def manifest():
    entries = {}
    pending = [""]
    while pending:
        rel_dir = pending.pop()
        with os.scandir(os.path.join(ROOT, rel_dir)) as it:
            for item in it:
                rel = f"{rel_dir}/{item.name}" if rel_dir else item.name
                if rel in SKIPPED:
                    continue
                st = item.stat(follow_symlinks=False)
                entries[rel] = record(rel, st)
                if entries[rel]["type"] == "dir" and rel not in PRUNED:
                    pending.append(rel)
    return entries


def scratch_base():
    """The scratch base, or None when absent. Neither data/ nor the base
    may be a symlink or a non-directory: clearing must not be redirected."""
    for rel in ("data", SCRATCH):
        try:
            st = os.lstat(os.path.join(ROOT, rel))
        except FileNotFoundError:
            return None
        if not stat.S_ISDIR(st.st_mode):
            fail(f"{rel} is a {kind(st.st_mode)}, not a directory; refusing to touch it")
    return os.path.join(ROOT, SCRATCH)


def make_searchable(path):
    st = os.lstat(path)
    if stat.S_ISDIR(st.st_mode):
        os.chmod(path, stat.S_IMODE(st.st_mode) | stat.S_IRWXU)


def remove_tree(path):
    """Remove one entry without following symlinks, first restoring owner
    read, write and search on every directory (a failed permission test can
    leave one read-only)."""
    st = os.lstat(path)
    if not stat.S_ISDIR(st.st_mode):
        os.unlink(path)
        return
    make_searchable(path)
    with os.scandir(path) as it:
        children = [item.path for item in it]
    for child in children:
        remove_tree(child)
    os.rmdir(path)


def clear_and_snapshot():
    base = scratch_base()
    if base is not None:
        failures = []
        with os.scandir(base) as it:
            entries = [item.path for item in it]
        for entry in entries:
            try:
                remove_tree(entry)
            except OSError as error:
                failures.append(f"  {os.path.relpath(entry, ROOT)}: {error}")
        if failures:
            fail("could not clear test scratch:\n" + "\n".join(failures))
    # Create the snapshot's parent before walking, so writing the snapshot
    # does not add a protected entry after the baseline.
    data = os.path.join(ROOT, "data")
    if not os.path.lexists(data):
        os.mkdir(data)
    snapshot = manifest()
    tmp = os.path.join(ROOT, SNAPSHOT_TMP)
    with open(tmp, "w", encoding="utf-8") as out:
        json.dump(snapshot, out, sort_keys=True)
    os.replace(tmp, os.path.join(ROOT, SNAPSHOT))
    print(f"test tree snapshot ok ({len(snapshot)} entries)")


def verify():
    path = os.path.join(ROOT, SNAPSHOT)
    try:
        with open(path, encoding="utf-8") as src:
            before = json.load(src)
    except (OSError, ValueError) as error:
        fail(f"no usable snapshot at {SNAPSHOT} ({error}); the pre-test stage did not record one")
    if not isinstance(before, dict):
        fail(f"malformed snapshot at {SNAPSHOT}")
    after = manifest()
    problems = []
    for rel in sorted(before.keys() | after.keys()):
        old, new = before.get(rel), after.get(rel)
        if old == new:
            continue
        if old is None:
            problems.append(f"  added    {rel}")
        elif new is None:
            problems.append(f"  removed  {rel}")
        else:
            fields = sorted(k for k in old.keys() | new.keys() if old.get(k) != new.get(k))
            problems.append(f"  changed  {rel} ({', '.join(fields)})")
    base = scratch_base()
    if base is not None:
        with os.scandir(base) as it:
            residue = sorted(item.name for item in it)
        problems.extend(f"  residue  {SCRATCH}/{name}" for name in residue)
    if problems:
        fail(
            "the test phase left changes in the checkout or residue in test scratch\n"
            "(a test wrote outside its scratch, retained a tree without removing it,\n"
            "or the tree was edited during the run):\n" + "\n".join(problems)
        )
    os.unlink(path)
    print("test tree unchanged")


def main():
    if len(sys.argv) != 2 or sys.argv[1] not in ("clear-and-snapshot", "verify"):
        print("usage: test_tree.py clear-and-snapshot|verify")
        sys.exit(2)
    if sys.argv[1] == "clear-and-snapshot":
        clear_and_snapshot()
    else:
        verify()


if __name__ == "__main__":
    main()
