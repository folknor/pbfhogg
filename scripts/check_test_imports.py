"""Hold the CLI tests to the library's fixture and test-infrastructure surface.

A CLI test proves what the `pbfhogg` binary does. It may name the library
only to build inputs, read outputs and spell the binary's test hooks; any
other library item it names couples it to an internal shape, or lets a
library call stand in for the binary. This check finds every `pbfhogg::`
path spelled in `crates/cli/tests/**/*.rs` and refuses one outside
`SURFACE` unless a `// library-deliberate: <reason>` line comment sits on
its line, alone on the line above it, or on the first line of its `use`
statement.

It is a direct-spelling review guard, lexical and fail-closed on use-tree
syntax it does not know. Every `use` statement is parsed as a tree (nested
braces, grouped roots, `self`, globs, renames, raw identifiers, comments
between tokens) and its leaves are checked; outside `use` statements every
path rooted at `pbfhogg` is checked. A module path (`writer`, `writer::*`,
`writer::{self}`) never passes; an item on the surface passes with any
further segments (variants, associated items). A rename of the crate and
`extern crate pbfhogg` are refused. Outside its promise: re-exports through
`crate::`, the helpers in `crates/lib/tests/common/` the CLI binary mounts
by path, macro expansion, and whether a test's oracle is independent of the
code under test (`reference/testing.md` owns that rule).
"""
from pathlib import Path
import re
import sys

from rust_source_shapes import blank_comments_and_literals

ROOT = 'pbfhogg'
MARKER = re.compile(r'//\s*library-deliberate:\s*\S')

# The one inventory of what a CLI test may name without a marker.
SURFACE = [tuple(path.split('::')) for path in (
    # Building inputs.
    'block_builder::BlockBuilder', 'block_builder::HeaderBuilder',
    'block_builder::MemberData', 'block_builder::Metadata',
    'writer::PbfWriter', 'writer::Compression',
    # Reading outputs.
    'Blob', 'BlobDecode', 'BlobError', 'BlobReader', 'BlobType', 'Element',
    'ElementReader', 'ErrorKind', 'HeaderBlock', 'HeaderOverrides',
    'MemberId', 'MemberType', 'Way', 'Info', 'DenseNodeInfo',
    'geocode_index::reader::Reader',
    # Test infrastructure: the hook protocol the invoker spells, and the
    # child-timeout bound.
    'test_hooks::PublishFault', 'test_hooks::PublishStep',
    'TEST_CHILD_TIMEOUT',
)]

TOKEN = re.compile(r"r#[A-Za-z_]\w*|[A-Za-z_]\w*|::|\S")


class Unsupported(Exception):
    pass


def line_comments(source, code):
    """(line, text) of every real `//` comment.

    `code` is `source` with comments blanked and literal contents masked, so
    a position whose masked character is not whitespace is code or literal
    and never starts a comment. Block comments nest, as in Rust.
    """
    found = []
    i, n = 0, len(source)
    while i < n:
        if not code[i].isspace():
            i += 1
        elif source.startswith('//', i):
            end = source.find('\n', i)
            end = n if end < 0 else end
            found.append((source.count('\n', 0, i) + 1, source[i:end]))
            i = end
        elif source.startswith('/*', i):
            depth, i = 1, i + 2
            while i < n and depth:
                if source.startswith('/*', i):
                    depth, i = depth + 1, i + 2
                elif source.startswith('*/', i):
                    depth, i = depth - 1, i + 2
                else:
                    i += 1
        else:
            i += 1
    return found


def tokens(code):
    """(text, line) tokens, raw identifiers normalized."""
    out = []
    for m in TOKEN.finditer(code):
        text = m[0][2:] if m[0].startswith('r#') else m[0]
        out.append((text, code.count('\n', 0, m.start()) + 1))
    return out


def ident(text):
    return re.fullmatch(r'[A-Za-z_]\w*', text) is not None


def parse_use(toks, i):
    """Parse the tree after `use` at toks[i].

    Returns ([(leaf, line)], index after `;`). A leaf is a tuple of
    segments; `self` collapses into its parent, a glob stays as a `*`
    segment, and a rename appends `as`. Its line is that of its last token.
    """
    leaves = []

    def at(i):
        if i >= len(toks):
            raise Unsupported('unterminated use')
        return toks[i]

    def tree(i, prefix):
        if at(i)[0] == '::':
            i += 1
        text, line = at(i)
        if text == '{':
            i += 1
            while True:
                if at(i)[0] == '}':
                    return i + 1
                i = tree(i, prefix)
                if at(i)[0] == ',':
                    i += 1
                elif at(i)[0] == '}':
                    return i + 1
                else:
                    raise Unsupported(f'use group token {at(i)[0]!r}')
        if text == '*':
            leaves.append((prefix + ('*',), line))
            return i + 1
        if not ident(text):
            raise Unsupported(f'use token {text!r}')
        i += 1
        path = prefix if text == 'self' else prefix + (text,)
        if at(i)[0] == '::':
            if text == 'self':
                raise Unsupported('use path continuing after self')
            return tree(i + 1, path)
        if at(i)[0] == 'as':
            name, line = at(i + 1)
            if not ident(name):
                raise Unsupported('use rename')
            leaves.append((path + ('as',), line))
            return i + 2
        leaves.append((path, line))
        return i

    i = tree(i, ())
    if at(i)[0] != ';':
        raise Unsupported(f'use statement ends at {at(i)[0]!r}')
    return leaves, i + 1


def allowed(leaf):
    path = leaf[:-1] if leaf[-1:] == ('as',) else leaf
    if path[:1] != (ROOT,):
        return True
    rest = path[1:]
    return bool(rest) and any(rest[:len(item)] == item for item in SURFACE)


def spelled(leaf):
    path = leaf[:-1] if leaf[-1:] == ('as',) else leaf
    return '::'.join(path) + (' as ...' if leaf[-1:] == ('as',) else '')


def violations(source):
    """(line, path) of every unmarked path outside the surface.

    Raises Unsupported, naming the line, on use-tree syntax it cannot read.
    """
    code = blank_comments_and_literals(source)
    lines = code.split('\n')
    marked = set()
    for line, text in line_comments(source, code):
        if MARKER.match(text):
            marked.add(line)
            # A marker on a line of its own covers the next line too.
            if not lines[line - 1].strip():
                marked.add(line + 1)
    toks = tokens(code)
    found = []
    i = 0
    while i < len(toks):
        text, line = toks[i]
        prev = toks[i - 1][0] if i else ''
        if text == 'use' and prev != '::':
            try:
                leaves, i = parse_use(toks, i + 1)
            except Unsupported as error:
                raise Unsupported(f'line {line}: {error}') from None
            found += [(own, spelled(leaf)) for leaf, own in leaves
                      if not allowed(leaf) and line not in marked and own not in marked]
            continue
        if text == 'extern' and [t for t, _ in toks[i + 1:i + 3]] == ['crate', ROOT]:
            if line not in marked:
                found.append((line, 'extern crate pbfhogg'))
            i += 3
            continue
        # A path rooted at the crate: not a segment of another path, though a
        # leading `::` (the extern prelude spelling) still roots it.
        rooted = prev != '::' or i < 2 or not ident(toks[i - 2][0])
        if text == ROOT and rooted and prev != '.':
            segments, j = [ROOT], i + 1
            while j + 1 < len(toks) and toks[j][0] == '::' and ident(toks[j + 1][0]):
                segments.append(toks[j + 1][0])
                j += 2
            if len(segments) > 1 and not allowed(tuple(segments)) and line not in marked:
                found.append((line, '::'.join(segments)))
            i = j
            continue
        i += 1
    return found


def audit(root):
    tests = root / 'crates/cli/tests'
    for name in ('invoker.rs', 'cli_sort.rs', 'cli_flag_contract.rs'):
        assert (tests / name).is_file(), f'CLI test corpus sentinel missing: {name}'
    paths = sorted(tests.rglob('*.rs'))
    errors = []
    for path in paths:
        try:
            for line, name in violations(path.read_text()):
                errors.append(f"{path.relative_to(root)}:{line}: {name} is outside the CLI tests' "
                              'library surface; drive the binary, or say why with '
                              '`// library-deliberate: <reason>`')
        except Unsupported as error:
            errors.append(f'{path.relative_to(root)}: unsupported use syntax, {error}')
    assert not errors, '\n'.join(errors)
    return len(paths)


if __name__ == '__main__':
    try:
        count = audit(Path(__file__).resolve().parents[1])
        print(f'{count} CLI test files scanned')
        print('test import audit ok')
    except (AssertionError, OSError) as error:
        sys.exit(str(error))
