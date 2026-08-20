# Bug hunt: passthrough and transform commands

Hunt date 2026-08-12. Single Opus hunter, scope: `src/commands/` (cat, sort,
repack, degrade, renumber, getid, getparents, inspect, tags_count, tags_filter,
check, export, mod.rs), `src/blob_meta/`, `src/tag_expr.rs`.

**All findings closed as of the round-3 fix pass, 2026-08-20.** The structural
recommendation is closed too: `pbfhogg check --index` exists, the indexdata /
tagdata trust boundary is documented in `CORRECTNESS.md`, and the standing
policy behind it is recorded as
[ADR-0013](../decisions/0013-blob-metadata-is-trusted-by-default-and-verified-on-demand.md).

The cold review of that fix pass found two further holes, both in the new
verifier itself and both of the same shape - it accepted a file it existed to
reject. Malformed indexdata was read as absent (the deserializer collapses
unreadable bytes to `None`, which is right for a reader and blind for a
verifier), and a valid tag index over a blob whose group mixes element kinds
was accepted although no such index can be complete. Both are closed, with
paired detection/honest-file tests at the unit and CLI level; the behaviour
matrix they pin is documented in `reference/cli-reference.md`.

This file is kept as an empty marker rather than deleted so a later round does
not re-file the same ground; delete it once the hunt series is retired.
