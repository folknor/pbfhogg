"""The repository-root path prefixes the crates/ move relocates.

Shared by scripts/textlint_coverage.py and scripts/rewrite_citations.py so
both map paths the same way. The move puts the library (with its src/,
tests/ and examples/) in crates/lib and the CLI and harness members in
crates/cli and crates/harness. Paths outside these prefixes (docs/, reference/, brokkr.d/, the root convention
files) stay where they are.
"""

MOVE = [
    ("src/", "crates/lib/src/"),
    ("tests/", "crates/lib/tests/"),
    ("examples/", "crates/lib/examples/"),
    ("cli/", "crates/cli/"),
    ("harness/", "crates/harness/"),
]


def moved(path):
    """`path` mapped through the move, or None if no prefix relocates it.
    A bare directory name (`src`, `cli`) maps like its trailing-slash form."""
    for old, new in MOVE:
        if path == old.rstrip("/"):
            return new.rstrip("/")
        if path.startswith(old):
            return new + path[len(old):]
    return None


def map_path(path):
    """`path` mapped through the move, unchanged if no prefix relocates it."""
    m = moved(path)
    return path if m is None else m
