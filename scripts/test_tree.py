#!/usr/bin/env python3
"""Test-run hygiene for `brokkr check`: clear test scratch before the test
phase, and prove no test leaked a scratch tree after it.

brokkr runs this as two `[[script_check]]` entries (brokkr.d/lints.toml):

  clear-and-snapshot  (pre-test)  empties data/test-scratch.
  verify              (post-test) requires data/test-scratch to be empty.

Clearing at the start of each checked run bounds residue: a failing test's
kept tree survives until the next run's pre-test stage, which is the
inspection window.

Only test scratch is judged. The checkout as a whole is not: editors, agents
and other tools write to it during a run, so a whole-tree comparison cannot
tell a test's write from theirs. Writes outside scratch are kept out by
construction instead (pbfhogg-test-scratch owns every test's scratch).

Not covered: `brokkr test` (it runs no script checks), and a run whose test
phase or coverage audit failed (brokkr skips post-test checks).

The sentinel is printed only after the step fully succeeded; brokkr ignores
the exit code.
"""

import os
import stat
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SCRATCH = "data/test-scratch"


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


def scratch_base():
    """The scratch base, or None when absent. data/ may be a symlink to a
    directory (hosts keep datasets on another drive); the scratch base
    itself must be a real directory, so clearing is never redirected."""
    try:
        st = os.stat(os.path.join(ROOT, "data"))
    except FileNotFoundError:
        return None
    if not stat.S_ISDIR(st.st_mode):
        fail(f"data resolves to a {kind(st.st_mode)}, not a directory; refusing to touch it")
    try:
        st = os.lstat(os.path.join(ROOT, SCRATCH))
    except FileNotFoundError:
        return None
    if not stat.S_ISDIR(st.st_mode):
        fail(f"{SCRATCH} is a {kind(st.st_mode)}, not a directory; refusing to touch it")
    return os.path.join(ROOT, SCRATCH)


def remove_tree(path):
    """Remove one entry without following symlinks, first restoring owner
    read, write and search on every directory (a failed permission test can
    leave one read-only)."""
    st = os.lstat(path)
    if not stat.S_ISDIR(st.st_mode):
        os.unlink(path)
        return
    os.chmod(path, stat.S_IMODE(st.st_mode) | stat.S_IRWXU)
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
    print("test tree snapshot ok")


def verify():
    base = scratch_base()
    if base is not None:
        with os.scandir(base) as it:
            residue = sorted(item.name for item in it)
        if residue:
            fail(
                "the test phase left residue in test scratch (a test retained a tree\n"
                "without removing it):\n" + "\n".join(f"  {SCRATCH}/{name}" for name in residue)
            )
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
