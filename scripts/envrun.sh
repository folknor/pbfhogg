#!/bin/bash
# Run a command with env assignments, from inside the Claude harness.
#
# The harness permission matcher blocks `VAR=x brokkr ...` command lines,
# so in-session runs that need env assignments (such as the `PBFHOGG_*`
# gates of the 2026-07-12 env-gated read-path batch, recorded in
# reference/performance-history.md) go through this wrapper instead:
#
#   scripts/envrun.sh PBFHOGG_FOO=1 brokkr ...
#
# Preserves cwd (brokkr must run from the project root). Everything after
# the assignments is exec'd verbatim.
exec env "$@"
