# BlobReader::seek_raw fix implementation

Working notes for the actual fix. Companion to `seek-raw-audit.md` (the
problem analysis and call-site inventory). This file captures the
implementation decisions, measurement data, and any surprises hit while
landing the fix.

Started 2026-04-18 at commit `ca6711e` (post regression-fix).

## Status

- [x] Audit call sites + SeekFrom usage
- [x] Design trait shape
- [x] Implement
- [x] Bench planet ALTW external
- [x] Bench Europe extract --smart / tags-filter / planet renumber
- [x] Docs + commit

## Audit findings

### Call sites (verified vs `seek-raw-audit.md`)

10 callers from the audit, all `BufReader<File>` via `seekable_from_path`,
all using `SeekFrom::Current(positive)` to skip a blob body. Verified by
re-walking the codebase post-split (extract.rs, inspect.rs, geocode/builder.rs,
renumber_external.rs are now subdirectories — line numbers in the audit
are stale but call sites still resolve).

Direct `BlobReader::seek_raw` callers:

| Site | File:line (current tip) | Notes |
|---|---|---|
| `build_classify_schedule` | uses `next_header_with_data_offset` (no direct seek_raw — internal to BlobReader) | indirect via iterator |
| `build_classify_schedules_split` | same | indirect |
| `tags_filter_single_pass` | uses `next_header_skip_blob` | indirect |
| `tags_filter_two_pass` | same | indirect |
| extract simple/complete/smart | `build_blob_schedule_with_passthrough` | indirect via iterator |
| `try_extract_multi_single_pass` | indirect | iterator |
| `scan_blob_metadata` | indirect | iterator |
| `build_all_blob_schedules` | indirect | iterator |
| `IndexedReader::create_index` | direct `seek_raw(SeekFrom::Current(data_size))` | (low-priority library API) |

The audit overcounted slightly: the production callers all go through
`BlobReader`'s iterator-style API (`next_header_skip_blob` /
`next_header_with_data_offset`), which internally calls `seek_raw`. Only
`IndexedReader::create_index` calls `seek_raw` directly. Either way the
underlying mechanism — `BufReader::seek` discarding the buffer on every
forward skip — is the same.

### SeekFrom variants in production paths

All production header-walk paths use `SeekFrom::Current(positive)`. No
absolute seeks (`Start`/`End`) and no backward seeks in the hot paths.

`IndexedReader::create_index` also uses `SeekFrom::Current(positive)` for
the body skip during index construction.

The `BlobReader<R: Read + Seek + Send>` impl exposes `seek_raw` as a public
method that accepts arbitrary `SeekFrom`, so any external library user
could pass a different variant — but the in-tree call set is uniform.

### Concrete reader types in use

- **`BufReader<File>`** — `BlobReader::seekable_from_path` (production
  hot paths), `tests/roundtrip_real.rs`, `cli/src/main.rs`. 256 KB buffer.
  This is the type that pays the discard cost.
- **`File`** (no BufReader) — `IndexedReader::from_path` constructs
  `IndexedReader<File>` directly. Comment at `src/read/indexed.rs:420-431`
  documents the intentional decision: random-seek workload doesn't benefit
  from BufReader because seeks would just discard buffered data anyway.
  No optimization needed here — there's no buffer to preserve.
- **`Cursor<&[u8]>` / `Cursor<&Vec<u8>>` / `Cursor<Vec<u8>>`** — in-memory
  PBF tests (`tests/roundtrip.rs`, `tests/corrupt_input.rs`). Already
  optimal — `Cursor::seek` is just a cursor-position bump, no fd cost,
  no buffer to preserve.
- **Library API:** `BlobReader<R>` is generic. Tightening the bound is a
  small breaking change for downstream library users with exotic R types
  (workaround: `impl BlobReaderSource for MyReader {}` — picks up the
  default `seek`-based impl, correct but slow).

### Direct vs indirect `seek_raw` calls

Production hot-path callers all go through `next_header_skip_blob` /
`next_header_with_data_offset`, which internally call
`seek_raw(SeekFrom::Current(header.datasize as i64))`. `IndexedReader::create_index`
also calls `next_header_skip_blob`, so even it goes through the iterator-
internal seek_raw.

The only direct external `seek_raw` calls are in `tests/read_paths.rs:294,300`
which test the `SeekFrom::Start(0)` and `SeekFrom::End(0)` semantics —
those need to keep working (and don't benefit from buffer preservation
since they're absolute seeks).

So the fix narrows to two internal call sites:
`src/read/blob.rs:1001` and `src/read/blob.rs:1046`.

## Design decisions

### Trait approach (vs inherent specialization)

Rust doesn't allow inherent method specialization. Two inherent impls of
the same struct can't define methods with the same name where the type
parameters overlap. So the audit's stated "specialize
`impl BlobReader<BufReader<R>>` with an override of `seek_raw`" approach
won't compile (verified mentally; not yet test-compiled).

The clean solution: a sealed trait that abstracts "skip forward without
losing buffered bytes" with one impl per concrete reader type.

### Trait shape

```rust
/// Underlying source for [`BlobReader`] that supports seeking.
///
/// Provides a fast path for relative skips that preserves any internal
/// buffer (e.g. `BufReader`'s read-ahead). The default `skip_relative`
/// implementation falls through to `Seek::seek(SeekFrom::Current(_))`,
/// which is correct but discards any buffer on `BufReader` — the cause
/// of the original regression. Override for buffered readers to keep
/// the buffer when the target lies within the buffered window.
pub trait BlobReaderSource: std::io::Read + std::io::Seek {
    fn skip_relative(&mut self, offset: i64) -> std::io::Result<()> {
        self.seek(std::io::SeekFrom::Current(offset)).map(|_| ())
    }
}

impl<R: std::io::Read + std::io::Seek> BlobReaderSource for std::io::BufReader<R> {
    fn skip_relative(&mut self, offset: i64) -> std::io::Result<()> {
        self.seek_relative(offset)
    }
}

impl BlobReaderSource for std::fs::File {}
impl<T: AsRef<[u8]>> BlobReaderSource for std::io::Cursor<T> {}
```

**Why `i64` not `u64`:** matches stdlib `BufReader::seek_relative`
signature; lets the trait be useful for the rare backward-seek case
without forcing a separate method. Production hot path always passes
positive offsets — no semantic change.

**Why no blanket `impl<R: Read + Seek> BlobReaderSource for R`:** Rust's
coherence rules disallow it once we have specific impls for `BufReader<R>`
(would overlap). On stable, no specialization. Each impl explicit.

**Why public, not sealed:** sealing would block downstream library users
who pass non-{`BufReader`,`File`,`Cursor`} readers from using `BlobReader`
at all. With public+default-impl, they add `impl BlobReaderSource for MyReader {}`
and get correct (slow) behavior. We pay one minor surface-area cost to
preserve library-user flexibility.

### Where the trait bound applies

- `BlobReader::new_seekable<R>(reader: R)` — bound widens from
  `R: Read + Seek + Send` to `R: BlobReaderSource + Send`.
- `IndexedReader::new<R>(reader: R)` — same widening.
- All inherent methods on the seekable `BlobReader<R>` impl, including
  `seek_raw`, `next_header_skip_blob`, `next_header_with_data_offset`.

Public API surface impact: the bound on the constructors. Library users
constructing `BlobReader<MyReader>` need to add a one-line impl. The
`from_path` / `seekable_from_path` / `IndexedReader::from_path` helpers
are unchanged because they return concrete `BlobReader<{BufReader<File>,File}>`.

### Where `skip_relative` gets used internally

The two internal call sites at `src/read/blob.rs:1001` and `:1046` switch
from `self.seek_raw(SeekFrom::Current(header.datasize as i64))` to a new
helper that calls `self.reader.skip_relative(...)` and updates the
running offset by adding `header.datasize`.

The public `seek_raw(SeekFrom)` keeps its existing behavior for
`SeekFrom::Start` / `SeekFrom::End` (no buffer preservation possible
without absolute-position tracking, and tests rely on the current shape).

## Measurements

(Filled in as benches complete.)

### Pre-fix sanity (post-regression-fix baseline)

The `cat --type way` planet number from the regression-fix verification
(commit `ca6711e`, UUID not stored — `--force` dirty tree): **43.9 s**.
This is the post-regression-fix baseline; everything below should beat
it where the seek_raw amplification was a meaningful fraction of wall.

### Post-fix benches

| Caller / Command | Dataset | Pre-fix | Post-fix | Δ | UUID |
|---|---|---:|---:|---:|---|
| TBD | TBD | TBD | TBD | TBD | TBD |
