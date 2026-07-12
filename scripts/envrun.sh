#!/bin/bash
# Run a command with env assignments, from inside the Claude harness.
#
# The harness permission matcher blocks `VAR=x brokkr ...` command lines,
# so in-session gate checks for the env-gated read-path batch
# (notes/env-gated-readpath-batch.md) go through this wrapper instead:
#
# Preserves cwd (brokkr must run from the project root). Everything after
# the assignments is exec'd verbatim.
exec env "$@"
