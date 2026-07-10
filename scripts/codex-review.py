#!/usr/bin/env python3
"""Spec-loop review role (reference/orchestrate.md): codex gpt-5.6-sol at xhigh,
no goal. The deepest reasoner in the system, spent critiquing a spec before
any code exists.

Usage: codex-review.py [--model MODEL] '<one-line prompt, single-quoted>'

Model defaults to codex_common.MODEL (gpt-5.6-sol); override with --model.
Prints a clean digest (final agent message, usage, any log lines). Never
resumes; the raw NDJSON stays inside the process.
"""
import argparse
import sys

from codex_common import MODEL, run_codex

if __name__ == "__main__":
    parser = argparse.ArgumentParser(
        description="Spec-loop review role (codex at xhigh, no goal)."
    )
    parser.add_argument(
        "--model",
        default=MODEL,
        help=f"codex model (default: {MODEL})",
    )
    parser.add_argument(
        "prompt",
        nargs="?",
        default="",
        help="one-line prompt, single-quoted",
    )
    args = parser.parse_args()
    sys.exit(run_codex(args.prompt, effort="xhigh", goal=False, model=args.model))
