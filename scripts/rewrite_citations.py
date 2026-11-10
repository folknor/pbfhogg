#!/usr/bin/env python3
"""Classify, and later rewrite, path citations the crates/ move relocates.

The move puts every workspace member under crates/. Every path-shaped token that starts with a moved
prefix (scripts/move_map.py: src/, tests/, examples/, cli/, harness/) in
the durable documents and in the members' Rust sources is put in one class:

  repo-root         names a path that exists in the repository now;
                    rewritten through the move map
  repo-root-link    the same, written relative to the citing file (a
                    Markdown link); rewritten as a new relative path
  package-relative  resolved by cargo or rustc against the package
                    (include_str!, #[path], CARGO_MANIFEST_DIR joins,
                    doctest inputs); kept, since it moves with its package
  external          names another project's file (brokkr, osmium, ...);
                    kept
  historical        names a path the repository once had and no longer
                    does, or sits in a dated record (CHANGELOG.md,
                    reference/performance-history.md); kept
  unclassified      none of the above; must be empty before a rewrite

    rewrite_citations.py [list] [--base REF] [--out FILE]
        dry run: every occurrence by class, with the rewrite each repo-root
        one would get; --out writes the listing to FILE and prints only the
        totals and the unclassified occurrences
    rewrite_citations.py check
        after the move: fail on any citation that still names a pre-move path
    rewrite_citations.py apply --base REF
        after the move: classify against REF, the last pre-move commit, and
        rewrite repo-root and repo-root-link occurrences in the moved files;
        a line edited since REF is reported for a rewrite by hand

Without --base, `list` reads the working tree and the index.

`list` ends with a per-class count and exits non-zero while anything is
unclassified. notes/ is not scanned: its documents are transient and the
plan itself cites the old layout on purpose. Bare `Cargo.toml` mentions are
not path citations of a moved prefix and are not listed; the root manifest
splits in the move and its citations are reviewed by hand.
"""
from pathlib import Path, PurePosixPath
import os
import re
import subprocess
import sys

sys.path.insert(0, str(Path(__file__).resolve().parent))
from move_map import MOVE, moved  # noqa: E402
from textlint_coverage import glob_regex  # noqa: E402

root = Path(__file__).resolve().parent.parent

PREFIXES = "|".join(re.escape(old.rstrip("/")) for old, _ in MOVE)
# A moved prefix followed by a slash and a path, or any `../`-relative form of
# one, which may stop at the bare directory (`"../src"`).
TOKEN = re.compile(
    rf"(?<![\w./~@:-])((?:\.\./)+(?:{PREFIXES})\b(?:/[\w./*<>-]*)?|(?:{PREFIXES})/[\w./*<>-]*)"
)
PLACEHOLDER = re.compile(r"<[^<>/]*>")
DOC_ROOT_FILES = {
    "AGENTS.md", "CLAUDE.md", "README.md", "LIBRARY.md", "LLM.md",
    "DEVIATIONS.md", "CORRECTNESS.md", "CHANGELOG.md",
}
DATED = {"CHANGELOG.md", "reference/performance-history.md"}
PACKAGE_MARKERS = ("include_str!", "include_bytes!", "#[path", "CARGO_MANIFEST_DIR")
EXTERNAL_HINTS = re.compile(
    r"brokkr|libosmium|osmium|osmosis|osmconvert|nidhogg|elivagar|traccar|"
    r"planetiler|research/|osmpbf|protobuf|geoparquet|codex",
    re.IGNORECASE,
)
# Old and new member prefixes: `list` and `apply` read the pre-move tree,
# `check` the moved one, and a scan that matched only one layout would pass
# blind on the other.
MEMBER_RS = tuple(old for old, _ in MOVE) + tuple(new for _, new in MOVE)


def git_lines(*args):
    out = subprocess.run(["git", *args], cwd=root, check=True, capture_output=True).stdout
    return [line for line in out.decode().split("\n") if line]


def scanned(files):
    for f in files:
        if f in DOC_ROOT_FILES or f in ("cli/README.md", "crates/cli/README.md"):
            yield f
        elif f.endswith(".md") and f.split("/")[0] in ("docs", "reference", "decisions"):
            yield f
        elif f.endswith(".rs") and f.startswith(MEMBER_RS):
            yield f


def comment_start(line):
    """Column of a `//` comment outside a string literal, or None."""
    in_str, i = False, 0
    while i < len(line):
        c = line[i]
        if in_str:
            if c == "\\":
                i += 1
            elif c == '"':
                in_str = False
        elif c == '"':
            in_str = True
        elif line.startswith("//", i):
            return i
        i += 1
    return None


class Index:
    """The tracked tree: the index, or the tree of commit `ref`."""

    def __init__(self, ref=None):
        self.ref = ref
        self.files = git_lines("ls-tree", "-r", "--name-only", ref) if ref else git_lines("ls-files")
        self.paths = set(self.files)
        self.dirs = {str(p) for f in self.files for p in PurePosixPath(f).parents}
        self.ever = set(git_lines("log", "--all", "--name-only", "--format="))
        self.ever_dirs = {str(p) for f in self.ever for p in PurePosixPath(f).parents}

    def exists(self, path):
        # A placeholder such as `cli_<command>.rs` stands for any name.
        path = PLACEHOLDER.sub("*", path.rstrip("/"))
        if any(c in path for c in "*?"):
            rx = glob_regex(path)
            return any(rx.match(f) for f in self.files)
        return path in self.paths or path in self.dirs

    def existed(self, path):
        path = PLACEHOLDER.sub("*", path.rstrip("/"))
        if any(c in path for c in "*?"):
            rx = glob_regex(path)
            return any(rx.match(f) for f in self.ever)
        return path in self.ever or path in self.ever_dirs

    def read(self, path):
        if self.ref is None:
            return (root / path).read_text()
        return subprocess.run(
            ["git", "show", f"{self.ref}:{path}"], cwd=root, check=True, capture_output=True
        ).stdout.decode()


def resolve(citing, token):
    """The repository path a `../`-relative token names from `citing`."""
    joined = os.path.normpath(os.path.join(os.path.dirname(citing), token))
    return None if joined.startswith("..") else joined


def classify(index, citing, line, col, token):
    """(class, repository path or None)."""
    if citing in DATED:
        return "historical", None
    if citing.endswith(".rs"):
        start = comment_start(line)
        code = line if start is None else line[:start]
        if any(m in code for m in PACKAGE_MARKERS) or token.endswith("test.osm.pbf"):
            return "package-relative", None
        if start is None or col < start:
            # Code, not a comment: a string a test prints (an assertion
            # message) reads against the package, cargo's cwd for tests.
            if citing.startswith("crates/"):
                package = "/".join(citing.split("/")[:2])
            elif citing.startswith(("cli/", "harness/")):
                package = citing.split("/")[0]
            else:
                package = ""
            if not token.startswith("../") and index.exists(os.path.join(package, token)):
                return "package-relative", None
            return "unclassified", None
    relative = token.startswith("../")
    path = resolve(citing, token) if relative else token
    if path is not None and index.exists(path):
        return ("repo-root-link" if relative else "repo-root"), path
    if EXTERNAL_HINTS.search(line):
        return "external", None
    if path is not None and index.existed(path):
        return "historical", None
    return "unclassified", None


def new_token(citing, token, cls, path):
    """The token's rewrite, or None if it is kept."""
    if cls == "repo-root":
        return moved(token)
    if cls == "repo-root-link":
        target = moved(path) or path
        new_citing = moved(citing) or citing
        rel = os.path.relpath(target, os.path.dirname(new_citing) or ".")
        return rel + ("/" if token.endswith("/") else "")
    return None


def occurrences(index):
    for citing in scanned(index.files):
        text = index.read(citing)
        for n, line in enumerate(text.split("\n"), 1):
            for m in TOKEN.finditer(line):
                token = m.group(1).rstrip(".")
                if token.startswith("../"):
                    target = resolve(citing, token)
                    if target is not None and moved(target) is None:
                        continue  # e.g. docs/guide's ../cli/ is docs/cli/, which stays
                cls, path = classify(index, citing, line, m.start(1), token)
                yield citing, n, m.start(1), token, cls, path


CLASSES = ["repo-root", "repo-root-link", "package-relative", "external", "historical", "unclassified"]


def list_mode(base, out):
    index = Index(base)
    by_class = {c: [] for c in CLASSES}
    for citing, n, _, token, cls, path in occurrences(index):
        by_class[cls].append((citing, n, token, new_token(citing, token, cls, path)))
    listing = []
    for cls in CLASSES:
        listing.append(f"== {cls} ({len(by_class[cls])})")
        for citing, n, token, new in by_class[cls]:
            arrow = f" -> {new}" if new is not None and new != token else ""
            listing.append(f"{citing}:{n}: {token}{arrow}")
    totals = "== totals: " + ", ".join(f"{c} {len(by_class[c])}" for c in CLASSES)
    listing.append(totals)
    if out is None:
        print("\n".join(listing))
    else:
        Path(out).write_text("\n".join(listing) + "\n")
        print(totals)
        for item in by_class["unclassified"]:
            print(f"unclassified: {item[0]}:{item[1]}: {item[2]}")
    if by_class["unclassified"]:
        print(f"{len(by_class['unclassified'])} unclassified citations", file=sys.stderr)
        return 1
    print("citations classified")
    return 0


def check_mode():
    """After the move: a token naming an old-layout path whose mapped form
    exists is obsolete."""
    index = Index()
    stale = []
    for citing, n, _, token, cls, _ in occurrences(index):
        if citing in DATED:
            continue  # a dated record names the layout of its date, on purpose
        if cls in ("repo-root", "repo-root-link", "unclassified", "historical") and not token.startswith("../"):
            target = moved(token)
            if target is not None and not index.exists(token) and index.exists(target):
                stale.append(f"{citing}:{n}: {token} is now {target}")
    if stale:
        print("\n".join(stale), file=sys.stderr)
        return 1
    print("no obsolete citations")
    return 0


def apply_mode(base):
    """Classify against the pre-move commit `base`, rewrite the moved files.

    Each citing file is read at `base` and written at its moved location. A
    line is rewritten only if it is unchanged since `base`; any other line
    holding a rewrite is reported, never guessed at."""
    if base is None or not (root / "crates/lib").is_dir():
        print("apply runs after the move, with --base naming the pre-move commit", file=sys.stderr)
        return 1
    index = Index(base)
    edits = {}
    for citing, n, col, token, cls, path in occurrences(index):
        new = new_token(citing, token, cls, path)
        if cls == "unclassified":
            print(f"{citing}:{n}: {token} is unclassified; classify it first", file=sys.stderr)
            return 1
        if new is not None and new != token:
            edits.setdefault(citing, []).append((n, col, token, new))
    conflicts, rewritten = [], 0
    for citing, items in sorted(edits.items()):
        base_lines = index.read(citing).split("\n")
        target = root / (moved(citing) or citing)
        lines = target.read_text().split("\n")
        # Right to left within a line, so earlier columns stay valid.
        for n, col, token, new in sorted(items, key=lambda e: (e[0], -e[1])):
            i = n - 1
            if i >= len(lines) or lines[i] != base_lines[i]:
                conflicts.append(f"{citing}:{n}: line changed since {base}; rewrite {token} -> {new} by hand")
                continue
            # Keep base in step, so a second rewrite on this line still matches.
            base_lines[i] = lines[i] = lines[i][:col] + new + lines[i][col + len(token):]
            rewritten += 1
        target.write_text("\n".join(lines))
    print("\n".join(conflicts), file=sys.stderr)
    print(f"rewrote {rewritten} citations, {len(conflicts)} left by hand")
    return 1 if conflicts else 0


def main(argv):
    options = {}
    for flag in ("--base", "--out"):
        if flag in argv:
            i = argv.index(flag)
            options[flag] = argv[i + 1]
            argv = argv[:i] + argv[i + 2:]
    base = options.get("--base")
    mode = argv[0] if argv else "list"
    if mode == "list":
        return list_mode(base, options.get("--out"))
    if mode == "check":
        return check_mode()
    if mode == "apply":
        return apply_mode(base)
    print(__doc__, file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
