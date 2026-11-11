#!/usr/bin/env python3
"""Source invariants that need a presence check or a function-scoped scan.

Textlint rules can only forbid a token on one physical line, so these four
cannot be textlint rules:

- The CLI dispatch is owned by finish_invocation, whose complete body runs
  the dispatch and then both counter hooks. Library textlint rules restrict
  other callers. This checks the fixed lifecycle shape, not Rust control flow.
- The complete open_probe_lock constructor is sealed to a noncreating,
  nontruncating read-write option chain, and probe_lock uses that constructor.
- No `as i64` cast inside the arguments of a `.counter(...)` call.
  `Invocation::counter` takes unsigned integers directly and saturates them
  at `i64::MAX`, where a cast wraps; rustfmt puts long arguments on their own
  line, so the call has to be read whole. A user-supplied u64 whose bit
  pattern is the meaning (degrade's drop seed) uses `cast_signed()`. The scan
  sees the call's arguments only, not casts made upstream of a local.
- No `static` item gated on the `test-hooks` feature anywhere in the
  library. Test pause and fault state rides the run's Invocation token
  (crates/lib/src/test_hooks.rs, reference/testing.md); a gated static is process-global
  hook state that reaches every concurrent test. The textlint rule
  `no-static-fault-hook-state` covers crates/lib/src/test_hooks.rs whole, but test-hook
  items also live with the code they reach (crates/lib/src/output.rs, crates/lib/src/host.rs), and
  the gate sits on the line above the item, which a one-line rule cannot
  read. The scan reads each static's own attribute block, so a static
  nested inside a gated module or macro body is not seen.

Comments are stripped before scanning so a commented-out call or a comment
that mentions a forbidden token cannot decide the outcome.
"""
from pathlib import Path
import re
import sys

from rust_source_shapes import blank_comments_and_literals, lifecycle_errors

root = Path(__file__).resolve().parent.parent
errors = []


EMIT_CALL = re.compile(r"\.\s*counter\s*\(")
AS_I64 = re.compile(r"\bas\s+(?:r#)?i64\b")


def counter_casts(src):
    """(line, argument text) for every `.counter(` call casting `as i64`."""
    code = blank_comments_and_literals(src)
    found = []
    for m in EMIT_CALL.finditer(code):
        depth, j = 1, m.end()
        while j < len(code) and depth:
            if code[j] == "(":
                depth += 1
            elif code[j] == ")":
                depth -= 1
            j += 1
        args = code[m.end() : j - 1]
        if AS_I64.search(args):
            found.append((code.count("\n", 0, m.start()) + 1, " ".join(args.split())))
    return found


SELF_TEST = [
    # (source, expected number of flagged calls)
    ('inv.counter("a", n as i64);', 1),
    ('inv.counter(\n    "a",\n    n as i64,\n);', 1),
    ('inv.counter ("a", (n) as\n i64);', 1),
    ('inv.counter("a", n as r#i64);', 1),
    ('inv.counter("a", n);', 0),
    ('inv.counter("a", n); // inv.counter("b", m as i64)', 0),
    ('/* inv.counter("b", m as i64) /* nested */ still */ inv.counter("a", n);', 0),
    ('inv.counter("a (", n); let x = m as i64;', 0),
    ('inv.counter(r#"a ) "# , n); let x = m as i64;', 0),
    ("inv.counter('(', n); let x = m as i64;", 0),
    ("fn f<'a>(x: &'a str) { inv.counter(x, n as i64); }", 1),
    ('inv.counter("a", count!(as "text" i64));', 0),
    ('inv.counter("a", v.map_or(-1, |b| i64::try_from(b).unwrap_or(i64::MAX)));', 0),
    ('let s = "inv.counter(\\"a\\", n as i64)";', 0),
]

for source, expected in SELF_TEST:
    got = len(counter_casts(source))
    if got != expected:
        errors.append(
            f"check_source_invariants.py self-test: expected {expected} flagged "
            f"call(s), got {got}, in {source!r}"
        )

def matching(pattern):
    """The files `pattern` selects under the repository root. A glob that
    selects nothing is an error, not a pass: after a layout change it would
    otherwise scan nothing and report the invariants ok."""
    paths = sorted(p for p in root.glob(pattern) if p.is_file())
    if not paths:
        errors.append(f"check_source_invariants.py: glob {pattern!r} matches no file")
    return paths


for pattern in ("crates/lib/src/**/*.rs", "crates/cli/src/**/*.rs"):
    for path in matching(pattern):
        for line, args in counter_casts(path.read_text()):
            errors.append(
                f"{path.relative_to(root)}:{line}: counter argument casts "
                f"`as i64` ({args}); pass the unsigned value, Invocation::counter saturates it"
            )


STATIC_ITEM = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?static\s")
TEST_HOOKS_GATE = re.compile(r'feature\s*=\s*"test-hooks"')


def gated_statics(src):
    """Line numbers of `static` items whose attribute block names the
    `test-hooks` feature. The block is the lines above the item up to the end
    of the previous statement or brace; the gate is read from the raw text,
    since blanking literals hides the feature name."""
    code = blank_comments_and_literals(src).splitlines()
    raw = src.splitlines()
    found = []
    for i, line in enumerate(code):
        if not STATIC_ITEM.match(line):
            continue
        j = i - 1
        while j >= 0:
            stripped = code[j].strip()
            if stripped.endswith((";", "{", "}")):
                break
            if stripped and TEST_HOOKS_GATE.search(raw[j]):
                found.append(i + 1)
                break
            j -= 1
    return found


GATED_STATIC_SELF_TEST = [
    # (source, expected number of flagged statics)
    ('#[cfg(feature = "test-hooks")]\nstatic X: u8 = 0;', 1),
    ('#[cfg(feature = "test-hooks")]\npub(crate) static X: u8 = 0;', 1),
    ('#[cfg(all(\n    test,\n    feature = "test-hooks"\n))]\n/// doc\nstatic X: u8 = 0;', 1),
    ('#[cfg(feature = "test-hooks")]\n\n#[allow(dead_code)]\nstatic X: u8 = 0;', 1),
    ("static SEQ: AtomicU64 = AtomicU64::new(0);", 0),
    ('#[cfg(feature = "test-hooks")]\nfn f() {}\nstatic X: u8 = 0;', 0),
    ('#[cfg(feature = "test-hooks")]\nuse a::b;\nstatic X: u8 = 0;', 0),
    ('// #[cfg(feature = "test-hooks")]\nstatic X: u8 = 0;', 0),
    ('#[cfg(feature = "other")]\nstatic X: u8 = 0;', 0),
    ('let s = "#[cfg(feature = \\"test-hooks\\")]";\nstatic X: u8 = 0;', 0),
]

for source, expected in GATED_STATIC_SELF_TEST:
    got = len(gated_statics(source))
    if got != expected:
        errors.append(
            f"check_source_invariants.py self-test: expected {expected} gated "
            f"static(s), got {got}, in {source!r}"
        )

for path in matching("crates/lib/src/**/*.rs"):
    for line in gated_statics(path.read_text()):
        errors.append(
            f"{path.relative_to(root)}:{line}: a static gated on the test-hooks "
            "feature is process-global hook state; put it in FaultPlan, carried by "
            "the run's Invocation token"
        )

cli = (root / "crates/cli/src/lib.rs").read_text()
scratch = (root / "crates/lib/src/scratch.rs").read_text()
for message in lifecycle_errors(cli, scratch):
    errors.append(f"source lifecycle: {message}")

if errors:
    print("\n".join(errors), file=sys.stderr)
    sys.exit(1)
print("source invariants ok")
