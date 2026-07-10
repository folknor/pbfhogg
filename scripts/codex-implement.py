#!/usr/bin/env python3
"""Spec-loop implement role (reference/orchestrate.md): codex gpt-5.5,
/goal-driven. The falsifiability test of the spec - an implementer working
only from the spec.

Usage: codex-implement.py [--effort LEVEL] [--model MODEL] '<one-line prompt, single-quoted>'

Effort defaults to medium - the standard falsifiability tier orchestrate.md
relies on (a medium-effort implementer just laying bricks). Pass
--effort xhigh to staff a sub-landing at a stronger implementer tier when the
plan explicitly calls for it (e.g. partial-region-jit-plan.md 2a/2b, justified
there by repeated non-delivery). Valid levels match codex
model_reasoning_effort: low, medium, high, xhigh.

Model defaults to gpt-5.5 - the implement tier; override with --model (e.g.
gpt-5.6-sol to staff a landing at the review-tier model).

The /goal prefix is added here, not by the caller. Prints a clean digest
(final agent message, usage, any log lines). Never resumes: if a run ends
with the goal unmet, launch a fresh codex-implement.py run instead.
"""
import argparse
import sys

from codex_common import run_codex

if __name__ == "__main__":
    parser = argparse.ArgumentParser(
        description="Spec-loop implement role (codex gpt-5.5, /goal-driven)."
    )
    parser.add_argument(
        "--effort",
        default="medium",
        choices=["low", "medium", "high", "xhigh"],
        help="codex reasoning effort (default: medium)",
    )
    parser.add_argument(
        "--model",
        default="gpt-5.5",
        help="codex model (default: gpt-5.5)",
    )
    parser.add_argument(
        "prompt",
        nargs="?",
        default="",
        help="one-line prompt, single-quoted",
    )
    args = parser.parse_args()
    sys.exit(run_codex(args.prompt, effort=args.effort, goal=True, model=args.model))
