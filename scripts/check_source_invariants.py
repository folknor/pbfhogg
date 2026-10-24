#!/usr/bin/env python3
"""Source invariants that need a presence check or a function-scoped scan.

Textlint rules can only forbid a token on one physical line, so these two
cannot be textlint rules:

- The CLI's exit path must call both counter-emission hooks. The library's
  textlint rules forbid emitting from anywhere else, which a program that
  emits nothing at all would satisfy, so the call itself is required here.
- `probe_lock` in src/scratch.rs must open the lock read-write and must never
  create it. The property is scoped to one function body.

Comments are stripped before scanning so a commented-out call or a comment
that mentions a forbidden token cannot decide the outcome.
"""
from pathlib import Path
import re
import sys

root = Path(__file__).resolve().parent.parent
errors = []


def code_only(source):
    return "\n".join(line.split("//")[0] for line in source.splitlines())


cli = re.sub(r"\s+", "", code_only((root / "cli/src/lib.rs").read_text()))
for hook in ("write::emit_writer_counters", "read::emit_read_counters"):
    call = f"pbfhogg::{hook}()"
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
