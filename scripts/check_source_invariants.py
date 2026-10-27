#!/usr/bin/env python3
"""Source invariants that need a presence check or a function-scoped scan.

Textlint rules can only forbid a token on one physical line, so these three
cannot be textlint rules:

- The CLI's exit path must call both counter-emission hooks. The library's
  textlint rules forbid emitting from anywhere else, which a program that
  emits nothing at all would satisfy, so the call itself is required here.
- `probe_lock` in src/scratch.rs must open the lock read-write and must never
  create it. The property is scoped to one function body.
- No `as i64` cast inside the arguments of a `.counter(...)` call.
  `Invocation::counter` takes unsigned integers directly and saturates them
  at `i64::MAX`, where a cast wraps; rustfmt puts long arguments on their own
  line, so the call has to be read whole. A user-supplied u64 whose bit
  pattern is the meaning (degrade's drop seed) uses `cast_signed()`. The scan
  sees the call's arguments only, not casts made upstream of a local.

Comments are stripped before scanning so a commented-out call or a comment
that mentions a forbidden token cannot decide the outcome.
"""
from pathlib import Path
import re
import sys

root = Path(__file__).resolve().parent.parent
errors = []


def blank_comments_and_literals(src):
    """Rust source with comments turned to spaces and the contents of string
    and char literals turned to `x`, offsets and newlines preserved.

    Comments vanish (whitespace); literals stay opaque tokens with their
    delimiters, so neither can supply or hide code, and blanking a literal
    cannot join the tokens around it into something that looks like code.
    Handles nested block comments, byte and C strings, raw strings with any
    number of hashes, char and byte literals, and tells lifetimes from chars.
    """
    out = list(src)
    n = len(src)
    i = 0

    def fill(start, end, ch):
        for k in range(start, end):
            if out[k] != "\n":
                out[k] = ch

    while i < n:
        c = src[i]
        if src.startswith("//", i):
            end = src.find("\n", i)
            end = n if end == -1 else end
            fill(i, end, " ")
            i = end
        elif src.startswith("/*", i):
            depth, j = 1, i + 2
            while j < n and depth:
                if src.startswith("/*", j):
                    depth, j = depth + 1, j + 2
                elif src.startswith("*/", j):
                    depth, j = depth - 1, j + 2
                else:
                    j += 1
            fill(i, j, " ")
            i = j
        elif (m := re.compile(r'(?:b|c)?r(#*)"').match(src, i)) and (
            i == 0 or not (src[i - 1].isalnum() or src[i - 1] == "_")
        ):
            close = '"' + m.group(1)
            end = src.find(close, m.end())
            end = n if end == -1 else end + len(close)
            fill(m.end(), end - len(close), "x")
            i = end
        elif c == '"' or (
            c in "bc"
            and src.startswith('"', i + 1)
            and (i == 0 or not (src[i - 1].isalnum() or src[i - 1] == "_"))
        ):
            start = i + 1 if c == '"' else i + 2
            j = start
            while j < n and src[j] != '"':
                j += 2 if src[j] == "\\" else 1
            fill(start, j, "x")
            i = j + 1
        elif c == "'" or (
            c == "b"
            and src.startswith("'", i + 1)
            and (i == 0 or not (src[i - 1].isalnum() or src[i - 1] == "_"))
        ):
            q = i if c == "'" else i + 1
            m = re.compile(r"'(?:\\(?:x[0-9a-fA-F]{2}|u\{[0-9a-fA-F]+\}|.)|[^\\'\n])'").match(src, q)
            if m:
                fill(q + 1, m.end() - 1, "x")
                i = m.end()
            else:
                i = q + 1  # a lifetime or label
        else:
            i += 1
    return "".join(out)


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

for pattern in ("src/**/*.rs", "cli/src/**/*.rs"):
    for path in sorted(root.glob(pattern)):
        for line, args in counter_casts(path.read_text()):
            errors.append(
                f"{path.relative_to(root)}:{line}: counter argument casts "
                f"`as i64` ({args}); pass the unsigned value, Invocation::counter saturates it"
            )


def code_only(source):
    return "\n".join(line.split("//")[0] for line in source.splitlines())


cli = re.sub(r"\s+", "", code_only((root / "cli/src/lib.rs").read_text()))
for hook in ("write::emit_writer_counters", "read::emit_read_counters"):
    call = f"pbfhogg::{hook}(&host.invocation)"
    if call not in cli:
        errors.append(
            f"cli/src/lib.rs: the CLI must call {call} on its exit path, "
            "or no run emits those counters"
        )

scratch = code_only((root / "src/scratch.rs").read_text())
parts = scratch.split("fn probe_lock(")
if len(parts) < 2:
    errors.append("src/scratch.rs: fn probe_lock not found")
else:
    probe = parts[1].split("\n}\n")[0]
    if ".write(true)" not in probe:
        errors.append("src/scratch.rs: probe_lock must open the lock read-write")
    if ".create(" in probe:
        errors.append("src/scratch.rs: probe_lock must never create LOCK")

if errors:
    print("\n".join(errors), file=sys.stderr)
    sys.exit(1)
print("source invariants ok")
