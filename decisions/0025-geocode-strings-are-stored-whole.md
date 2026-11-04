# ADR-0025: Geocode strings are stored whole

Date: 2026-11-04
Status: Accepted

## Decision

The geocode index stores every string it answers with as the tag value
whole. `strings.bin` entries are a canonical unsigned LEB128 byte length
followed by the UTF-8 bytes, with no terminator; a record names an entry by
the offset of its length prefix, and offset 0 is the one-byte length-zero
entry that every absent optional field names. A PBF tag value may hold a NUL
byte (PBF strings are length-delimited; only OSM XML cannot carry one), so a
NUL-terminated pool published such a value as its prefix: `10\0a` answered
as `10`, with nothing in the index to say so. The encoding is defined once,
in `src/geocode_index/format.rs`, and the builder's pool, the interpolation
resolver and the reader all go through it. Open checks that each referenced
offset frames a whole UTF-8 entry inside the pool and that the sentinel is
present; it does not check that an offset is the start of an entry the
builder wrote, since an offset naming another valid entry is undetectable
either way.

## Alternatives considered

- Refuse the build when an interned value holds a NUL, as the u16 count caps
  do (CORRECTNESS.md). Rejected: the limit was the format's own, not the
  input's, and removing it costs one prefix byte in place of one terminator
  byte for any string under 128 bytes; a refusal would abort a whole build
  over a value the index can hold.
- Treat such a value as absent at the tag loops, skipping the record or the
  field with a warning. Rejected: it drops valid input and spreads one
  encoding limit across every field the builder reads.
- Replace or cut the NUL. Rejected: it publishes a string that is not in the
  input.
- Keep NUL termination and a builder-side record of truncated offsets for
  callers that must not act on a prefix (the interpolation resolver's earlier
  guard). Rejected: every other reader, the query answers included, still saw
  the prefix.
