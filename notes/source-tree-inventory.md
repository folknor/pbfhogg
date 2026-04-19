# Source tree inventory (Stage 1 step 1 + 2)

Inventory of every non-command source file in `src/`, plus the shared infrastructure currently misfiled under `src/commands/`. Produced by parallel subagent fan-out, consolidated here. Drives the categorization step that follows.

Scope: `src/*.rs` (top-level), `src/read/*`, `src/write/*`, `src/geocode_index/*` (and `builder/*`), and the eight files in `src/commands/` that are believed to be shared library infrastructure (id_set_dense, tag_expr, external_radix, node_scanner, way_scanner, elements_pbf, elements_xml, stream_merge).

Excluded: command-specific files in `src/commands/` (Stage 2).

Total: 47 files, ~24K lines.

---

## Top-level `src/`

### `src/lib.rs` (177 lines)

**Purpose:** Crate root. Declares the public and private module tree and re-exports selected types and modules to provide a clean public API (`Element`, `ElementReader`, `BlobReader`, `PrimitiveBlock`, etc.) while routing internal modules (`blob_index`, `error`, `reorder_buffer`) as private dependencies. Centralizes crate-level docs with usage examples for sequential and parallel reading/writing.

**API surface:**
- Public module re-exports: `read`, `write`, `geo`, `osc`, `debug`, `geocode_index`, `commands`
- Public type re-exports: `Element`, `ElementReader`, `BlobReader`, `BlobFilter`, `BlobBbox`, `PrimitiveBlock`, `Error`, `ErrorKind`, `BlobError`, `Result`
- Public function re-exports: command functions (`cat`, `merge`, `extract`, `sort`, etc.) and header utilities

**Inbound deps:** None (crate root).

**Outbound deps:** None (declarations + re-exports only).

---

### `src/error.rs` (162 lines)

**Purpose:** Error type definitions for the crate. Defines the boxed `Error` enum wrapping `ErrorKind` (IO, UTF-8 stringtable decode, index bounds, blob decode, protobuf wire format, missing header). Implements `Display`, `StdError`, and `From<io::Error>` manually (no `thiserror` dependency).

**API surface:**
- `pub type Result<T>` - alias with pbfhogg::Error
- `pub struct Error` - opaque boxed error wrapper
- `pub enum ErrorKind` - Io, StringtableUtf8, StringtableIndexOutOfBounds, Blob, WireFormat, MissingHeader
- `pub enum BlobError` - InvalidHeaderSize, HeaderTooBig, MessageTooBig, Empty, InvalidDataSize
- `pub fn Error::kind() / into_kind()`

**Inbound deps:** lib.rs (re-exports); read/, write/, commands/ pervasively use `Result<T>`.

**Outbound deps:** std only (io, fmt, str, error).

---

### `src/debug.rs` (111 lines)

**Purpose:** Profiler marker and counter emission to a sidecar FIFO (`BROKKR_MARKER_FIFO` env var) with microsecond timestamps. Provides glibc `mallinfo2()` wrapper and Linux page-fault reading from `/proc/self/stat`. All functions no-op when the FIFO is absent.

**API surface:**
- `pub fn emit_marker(name: &str)`
- `pub fn emit_counter(name: &str, value: i64)` (prefixed with `@`)
- `pub fn emit_mallinfo2(prefix: &str)` (Linux only)
- `pub fn read_page_faults() -> (u64, u64)` (Linux only)

**Inbound deps:** geocode_index/builder/mod.rs; commands/time_filter.rs (test). Likely also referenced via `crate::debug::emit_*` at many call sites in write/, commands/altw, etc.

**Outbound deps:** std + libc (Linux).

---

### `src/reorder_buffer.rs` (118 lines)

**Purpose:** Sequence-number reorder buffer for out-of-order producer results. Holds items keyed by monotonic sequence numbers, yields contiguous ready items. Used by pipelined block reading and parallel writing to enforce output order despite multi-threaded processing. Panics on stale or duplicate inserts.

**API surface:**
- `pub(crate) struct ReorderBuffer<T>`
- `pub(crate) fn with_capacity(cap) -> Self`
- `pub(crate) fn push(seq, item)`
- `pub(crate) fn pop_ready() -> Option<T>`
- `pub(crate) fn pending_len() -> usize`

**Inbound deps:** read/pipeline.rs, write/writer.rs, write/uring_writer.rs, commands/merge/rewrite.rs.

**Outbound deps:** std (VecDeque).

---

### `src/geo.rs` (643 lines)

**Purpose:** Shared geometry utilities: ray-casting point-in-polygon (with antimeridian handling), polygon-with-holes containment, Douglas-Peucker simplification (with vertex cap), approximate cosine-projection distance metrics, greedy closed-ring assembly from way segments. Coordinates accept both degrees (general geometry) and decimicrodegrees i32 (OSM native). Used by geocode index and spatial extract.

**API surface:**
- `pub fn point_in_ring / point_in_ring_with_antimeridian / point_in_polygon`
- `pub fn ring_crosses_antimeridian / signed_area`
- `pub fn approx_distance_sq / meters_to_radians_sq / e7_to_rad / point_to_segment_distance_sq`
- `pub fn simplify_ring(ring, max_vertices)`
- `pub fn assemble_rings(segments)`
- `pub const EARTH_RADIUS_M: f64`

**Inbound deps:** geocode_index/reader.rs, geocode_index/builder/admin.rs, commands/extract/mod.rs.

**Outbound deps:** std only.

---

### `src/blob_index.rs` (1225 lines)

**Purpose:** Lightweight protobuf scanner and blob-level metadata format (v1 26 bytes, v2 42 bytes) embedded into BlobHeader fields. Enables fast blob filtering without full decompression: element type, ID range, count, spatial bbox. Scans DenseNodes/Way/Relation wire format using protohoggr primitives. Provides `BlobFilter` API for type/spatial/tag filtering of blob streams. Powers index scanning in merge/sort and block-level filtering in pipelined reads.

**API surface:**
- `pub struct BlobBbox` (+ `new`, `contains`, `intersects`)
- `pub struct BlobFilter` (+ `new`, `only_*`, `with_node_bbox`, `with_required_tag_keys`, `with_required_tag_prefixes`)
- `pub(crate) fn BlobFilter::wants / wants_index / wants_tag_index`
- `pub(crate) struct BlobIndex` (+ `serialize`, `deserialize`)
- `pub(crate) fn scan_block_ids(raw: &[u8]) -> Option<BlobIndex>`
- `pub(crate) fn scan_block_tags(raw: &[u8]) -> Option<TagIndex>`
- `pub(crate) enum ElemKind`

**Inbound deps:** lib.rs; read/pipeline.rs, read/reader.rs; write/writer.rs, write/block_builder.rs; commands/diff.rs, commands/merge_pbf.rs, commands/sort.rs, commands/stream_merge.rs, commands/cat.rs, commands/merge/*, commands/inspect/*.

**Outbound deps:** crate::read::elements (MemberType); protohoggr (Cursor, zigzag); std.

---

### `src/osc.rs` (1506 lines)

**Purpose:** Parser and arena-backed data structure for OSM change files (.osc.gz). Parses XML create/modify/delete actions into a `CompactDiffOverlay` using arena-packed binary layouts with interned tag keys and roles. Tracks created/modified/deleted nodes/ways/relations separately with zero-copy accessors. Loads and merges multiple diff files sorted by sequence number. ~40-60% memory reduction vs heap-based alternatives at planet scale.

**API surface:**
- `pub struct CompactDiffOverlay` (+ get/has/iter/count for nodes/ways/relations, `deleted_*: HashSet<i64>`)
- `pub struct CompactNodeRef<'a>` / `CompactWayRef<'a>` / `CompactRelationRef<'a>` (zero-copy accessors)
- `pub fn parse_osc_file(path) -> ParseResult<CompactDiffOverlay>`
- `pub fn parse_osc_file_into(path, overlay) -> ParseResult<()>`
- `pub fn load_all_diffs(diffs_dir) -> ParseResult<CompactDiffOverlay>`

**Inbound deps:** lib.rs; commands/merge/diff_ranges.rs, commands/merge/rewrite.rs, commands/merge/node_locations.rs.

**Outbound deps:** crate::read::elements (MemberType); flate2, quick_xml, rustc_hash; std.

---

## `src/read/`

### `src/read/mod.rs` (12 lines)

**Purpose:** Module root. Declares blob, block, dense, direct_reader, elements, file_reader, indexed, reader as public; columnar, pipeline, wire as `pub(crate)`.

**API surface:** Module declarations only.

**Inbound deps:** None (root).

**Outbound deps:** None.

---

### `src/read/blob.rs` (1449 lines)

**Purpose:** PBF blob envelope decode (outer container). Blob header parsing, zlib/zstd decompression with thread-local decompressor reuse and pooled buffer recycling. Sequential blob iteration via `BlobReader`. Contains the `DecompressPool` that prevents 10+ GB allocator-retained memory on large files.

**API surface:**
- `pub enum BlobType<'a>` / `pub enum BlobDecode<'a>`
- `pub struct ByteOffset` (offset tracking for seekable I/O)
- `pub struct Blob` / `pub struct BlobHeader` / `pub struct BlobReader<R: Read + Send>`
- `pub const MAX_BLOB_HEADER_SIZE / MAX_BLOB_MESSAGE_SIZE`
- `pub(crate) struct DecompressPool` + `pool_get_pub`, `pool_wrap`

**Inbound deps:** read/block.rs, read/reader.rs, read/indexed.rs, read/pipeline.rs; commands/way_scanner.rs, commands/node_scanner.rs; osc.rs.

**Outbound deps:** crate::read::block, crate::read::file_reader, crate::error, crate::blob_index.

---

### `src/read/block.rs` (927 lines)

**Purpose:** Decoded `PrimitiveBlock` and `HeaderBlock` representations (post-decompression). Block-type classification, element grouping, lazy iteration over primitive groups. Eager stringtable UTF-8 validation enabling fast unchecked lookups. Transmutes WireBlock lifetime for self-referential ownership inside a single Bytes buffer.

**API surface:**
- `pub struct HeaderBlock` / `pub struct HeaderBBox`
- `pub enum BlockType` (DenseNodes, Nodes, Ways, Relations, Mixed, Empty)
- `pub struct PrimitiveBlock` (not Clone)
- `pub struct PrimitiveGroup<'a>`
- Iterators: `BlockElementsIter`, `GroupIter`, `GroupNodeIter`, `GroupWayIter`, `GroupRelationIter`

**Inbound deps:** read/blob.rs, read/reader.rs, read/indexed.rs, read/pipeline.rs.

**Outbound deps:** crate::read::dense, crate::read::elements, crate::read::wire, crate::error.

---

### `src/read/columnar.rs` (177 lines)

**Purpose:** Batch-decodes dense nodes into columnar (contiguous arrays) for vectorization-friendly spatial classification. Decodes delta-encoded IDs and coordinates into separate `Vec<i64>` / `Vec<i32>` arrays for branchless multi-bbox filtering and autovectorizable tight loops.

**API surface:**
- `pub(crate) struct DenseNodeColumns`
- `pub(crate) fn clear / decode_append / collect_matching_ids_bbox / collect_matching_ids_multi_bbox`

**Inbound deps:** None directly grep'd (used inside read/pipeline.rs and commands/extract/multi.rs via re-export or internal use).

**Outbound deps:** crate::read::wire (PackedSint64Iter, WireDenseNodes).

---

### `src/read/dense.rs` (427 lines)

**Purpose:** Decodes and iterates DenseNodes (delta-encoded packed arrays for ID/lat/lon). Provides `DenseNode` element type, `DenseNodeIter`, optional metadata access. Coordinate conversion macro for nano/decimicro consistency.

**API surface:**
- `pub struct DenseNode<'a>` / `DenseNodeIter<'a>` / `DenseNodeInfo<'a>` / `DenseNodeInfoIter<'a>` / `DenseTagIter<'a>` / `DenseRawTagIter<'a>`

**Inbound deps:** read/block.rs.

**Outbound deps:** crate::read::block (stringtable helpers), crate::read::wire (multiple iters/types), crate::error.

---

### `src/read/direct_reader.rs` (270 lines)

**Purpose:** O_DIRECT page-aligned I/O bypassing the kernel page cache (Linux). Prevents cache pollution when reading planet-scale (80+ GB) PBFs. Custom memory-aligned buffer management; `libc::read()` with 4096-byte page alignment.

**API surface:**
- `pub struct DirectReader`
- `pub fn open(path)`
- `pub(crate) fn raw_fd / skip`
- `impl Read for DirectReader`

**Inbound deps:** read/file_reader.rs (feature-gated `linux-direct-io`).

**Outbound deps:** std + libc only.

---

### `src/read/elements.rs` (692 lines)

**Purpose:** High-level element types (`Node`, `Way`, `Relation`, `Element` enum) and their iterators. Lazy decode of tags, refs, member data, metadata from wire format. Coordinate conversions (nano-degrees to decimicrodegrees). Both typed and raw (stringtable-index) tag/member access.

**API surface:**
- `pub enum Element<'a>` / `pub struct Node / Way / Relation`
- `pub struct WayRefIter / WayNodeLocation / WayNodeLocationsIter`
- `pub enum MemberType / MemberId`
- `pub struct RelMember<'a> / RelMemberIter<'a>`
- `pub struct TagIter<'a> / RawTagIter<'a> / Info<'a>`
- `#[macro_export] macro_rules! impl_coordinate_conversions`

**Inbound deps:** read/block.rs, read/indexed.rs, read/reader.rs.

**Outbound deps:** crate::read::block, crate::read::dense, crate::read::wire, crate::error.

---

### `src/read/file_reader.rs` (111 lines)

**Purpose:** Abstracts file I/O strategy (buffered vs O_DIRECT) behind a single enum, optimized away when `linux-direct-io` is off. Includes POSIX_FADV_SEQUENTIAL kernel readahead advice.

**API surface:**
- `pub enum FileReader` (Buffered or Direct)
- `pub fn buffered / direct / open(path, direct)`
- `pub(crate) fn skip / raw_fd`
- `impl Read for FileReader`

**Inbound deps:** read/blob.rs, read/reader.rs.

**Outbound deps:** crate::read::direct_reader (feature-gated).

---

### `src/read/indexed.rs` (465 lines)

**Purpose:** Index-based filtering for high-volume extraction. Lazy in-memory index of blob offsets and ID ranges per blob, used to skip blobs that cannot contain matches. Bbox and ID-based filtering with a `BTreeSet` of target node IDs to find way dependencies.

**API surface:**
- `pub struct IdRanges`
- `pub struct IndexedReader<R: BlobReaderSource + Send>` (+ `new`, `create_index`, `filter_ways`, `filter_nodes`, `from_path`)

**Inbound deps:** None directly grep'd (likely used by extract simple/smart and getid).

**Outbound deps:** crate::error, crate::blob (BlobReader, BlobReaderSource, BlobType, ByteOffset), crate::block (PrimitiveBlock), crate::elements (Element, Way), crate::commands::id_set_dense.

---

### `src/read/pipeline.rs` (254 lines)

**Purpose:** Three-stage pipelined reader: I/O thread (raw blobs), rayon decode pool (parallel zlib + protobuf parse), reorder buffer (file-order delivery). Blob-level filtering to skip non-matching blobs. Channel backpressure between stages to bound memory.

**API surface:**
- `pub(crate) struct PipelineConfig` / `DEFAULT_READ_AHEAD` / `DEFAULT_DECODE_AHEAD`
- `pub(crate) fn run_pipeline<R, F>(...) -> Result<()>`

**Inbound deps:** read/reader.rs.

**Outbound deps:** crate::blob, crate::block, crate::blob_index, crate::error, crate::reorder_buffer, crate::commands::node_scanner.

---

### `src/read/reader.rs` (575 lines)

**Purpose:** High-level user-facing reader API. Wraps `BlobReader`. Sequential, pipelined, and parallel-map iteration over elements or blocks. Header parsing, thread pool config, pipeline buffering. Three consumption modes: simple (1 thread), pipelined (3 threads + rayon), `par_map_reduce` (all rayon).

**API surface:**
- `pub struct ElementReader<R: Read + Send>`
- `pub fn new / from_path / header / decode_threads / read_ahead / decode_ahead / with_blob_filter`
- `pub fn for_each / for_each_pipelined / for_each_block_pipelined / into_blocks_pipelined / par_map_reduce`
- `pub struct PipelinedBlocks`

**Inbound deps:** None (public library entry point; lib.rs re-export).

**Outbound deps:** crate::blob, crate::block, crate::elements, crate::file_reader, crate::pipeline, crate::blob_index, crate::error.

---

### `src/read/wire.rs` (688 lines)

**Purpose:** Low-level protobuf wire-format parsers for OSM PBF message types (`PrimitiveBlock`, `PrimitiveGroup`, `Node`, `Way`, `Relation`, `DenseNodes`, `DenseInfo`, `Info`, `StringTable`). Lazy group scanning, zero-copy buffer-relative offset pairs (no heap-allocated Vecs). Re-exports protohoggr primitives (`Cursor`, packed iterators).

**API surface:**
- `pub(crate) use protohoggr::{Cursor, Packed*Iter}`
- `pub(crate) struct WireStringTable / WireBlock / WireBlockMeta / WireGroup / WireMessageIter / WireDenseNodes / WireDenseInfo / WireNode / WireWay / WireRelation / WireInfo`

**Inbound deps:** read/blob.rs, read/block.rs, read/columnar.rs, read/dense.rs, read/elements.rs.

**Outbound deps:** crate::error.

---

## `src/write/`

### `src/write/mod.rs` (28 lines)

**Purpose:** Module root. Declares `block_builder`, `writer`, `file_writer` as public; `buf_pool`, `metrics`, `raw_passthrough`, `direct_writer`, `uring_writer` as `pub(crate)`. Defines page-aligned allocation utilities shared by Direct/Uring writers.

**API surface:**
- Module declarations
- `const PAGE_SIZE` (4096)
- `fn alloc_page_aligned()` (unsafe page-aligned heap alloc)

**Inbound deps:** lib.rs (re-exports `block_builder`, `writer`); commands/getid.rs.

**Outbound deps:** None (root).

---

### `src/write/block_builder.rs` (1712 lines)

**Purpose:** Serializes OSM elements into PBF `PrimitiveBlock` protobuf messages. Per-block string table interning, delta encoding for coords/refs, dense node packing, optional metadata. Tracks element counts, ID ranges, bboxes for blob index. Produces borrowed and owned serialized bytes with pre-computed `BlobIndex` (no writer-side scan). Supports merge passthrough via pre-seeded string tables. Exports `HeaderBuilder` for OSM headers (bbox, replication metadata, feature flags).

**API surface:**
- `pub struct BlockBuilder` (+ `new`, `is_empty`, `should_flush`, `can_add_*`, `add_node`, `add_way`, `add_way_with_locations`, `add_relation`)
- `pub(crate) fn pre_seed_string_table / is_pre_seeded`
- `pub fn take() / pub(crate) fn take_owned() / take_owned_swap()`
- `pub struct Metadata<'a>` / `pub struct MemberData<'a>`
- `pub struct HeaderBuilder<'a>` (+ `new`, `from_header`, `bbox`, `replication_*`, `sorted`, `optional_feature`, `historical`, `writing_program`, `build`)

**Inbound deps:** commands/getid.rs (BlockBuilder, MemberData, OwnedBlock); write/writer.rs (docstring + blob_index access).

**Outbound deps:** crate::blob_index (BlobIndex, ElemKind), crate::elements (MemberId, MemberType), protohoggr.

---

### `src/write/buf_pool.rs` (150 lines)

**Purpose:** Bounded free-list pool for `Vec<u8>` block builder buffers. Solves alloc churn in high-throughput snapshot filtering: instead of alloc/free of ~500 KB Vecs per block (heap fragmentation, RSS bloat), buffers cycle through the pool. Tracks hit/miss/capacity stats. Designed for worker-to-writer handoff (worker pulls cleared Vec, fills via `take_owned_swap`, writer returns after rayon compression).

**API surface:**
- `pub(crate) struct BlockBufPool` (+ `new`, `get`, `put`, `emit_counters`)

**Inbound deps:** write/writer.rs.

**Outbound deps:** crate::debug.

---

### `src/write/direct_writer.rs` (228 lines)

**Purpose:** O_DIRECT writer that bypasses kernel page cache. Page-aligned (address, size, file offset) writes. 256 KB internal page-aligned buffer. Final flush zero-pads tail to page boundary, writes, then `ftruncate` to logical size. Prevents cache pollution at planet-scale writes.

**API surface:**
- `pub struct DirectWriter` (+ `create`, `pub(crate) sync_all`)
- `impl Write`

**Inbound deps:** write/file_writer.rs.

**Outbound deps:** super::alloc_page_aligned.

---

### `src/write/file_writer.rs` (116 lines)

**Purpose:** Enum wrapper for buffered + O_DIRECT (feature-gated) writers via a single concrete type, with the match optimized away when `linux-direct-io` is off. Records counters via `WRITER_METRICS`. `flush_and_raw_fd()` returns RawFd for `copy_file_range` passthrough (incompatible with O_DIRECT).

**API surface:**
- `pub enum FileWriter` (+ `buffered`, `direct`)
- `pub(crate) fn flush_and_raw_fd`
- `impl Write`

**Inbound deps:** write/writer.rs.

**Outbound deps:** super::direct_writer, super::metrics::WRITER_METRICS.

---

### `src/write/metrics.rs` (129 lines)

**Purpose:** Atomic counters for the write pipeline: permit contention, frame/compress time, queuing, reorder high-water, I/O time, payload framing/raw/copy-range stats, buffered/direct write calls, fsync time, io_uring submissions. Static singleton (`WRITER_METRICS`) with compare-exchange high-water tracking. Emitted at flush via `emit()` calling `crate::debug::emit_counter()`.

**API surface:**
- `pub(crate) struct WriterMetrics`
- `pub(crate) static WRITER_METRICS: WriterMetrics`
- `pub fn record_reorder_high_water`
- `pub fn emit`

**Inbound deps:** write/file_writer.rs, write/writer.rs, write/uring_writer.rs.

**Outbound deps:** crate::debug.

---

### `src/write/raw_passthrough.rs` (89 lines)

**Purpose:** Scaffolding for per-group raw protobuf passthrough (currently `#[allow(dead_code)]`). Assembles a `PrimitiveBlock` from raw `StringTable` bytes + pre-encoded group bytes, skipping decode/re-encode for fully selected groups. Notes two viable approaches for mixing raw + re-encoded groups in one blob (string-table-aligned re-encode vs split output).

**API surface:**
- `pub(crate) fn frame_raw_block(...)`

**Inbound deps:** None (scaffolding, currently dead).

**Outbound deps:** protohoggr.

---

### `src/write/uring_writer.rs` (843 lines)

**Purpose:** io_uring writer thread for pipelined PBF output, replacing the synchronous writer thread when `linux-io-uring` is enabled. O_DIRECT + `WriteFixed` SQEs, pre-registered page-aligned buffers (64 x 256 KB = 16 MB) for max throughput on I/O-bound workloads (e.g., `Compression::None` on erofs). Reorder buffer for receipt-order output. Free-list allocation for registered buffer indices. Linked `ReadFixed`-`WriteFixed` chains for zero-copy passthrough via `copy_file_range`. Drains in-flight, reaps CQEs for short writes / index recycling. Linux 5.1+.

**API surface:**
- `pub(crate) fn uring_writer_thread(...)` (thread entry; internal structs only)

**Inbound deps:** write/writer.rs (`PbfWriter::to_path_uring`).

**Outbound deps:** crate::write::writer (OutputChunk, PipelineItem, WRITE_AHEAD), crate::reorder_buffer, super::alloc_page_aligned, super::metrics::WRITER_METRICS, io_uring crate.

---

### `src/write/writer.rs` (1356 lines)

**Purpose:** PBF file writer producing valid `.osm.pbf` (4-byte BE header length, BlobHeader, compressed Blob). Sync (single-threaded) and pipelined (rayon parallel + dedicated writer thread) modes. All three compressions: None, Zlib (default 6), Zstd (default 3). Block compression dispatched to rayon with a counting-semaphore permit pool to prevent OOM. Reorder buffer in writer thread sequences output. Tracks BlobIndex + optional tagdata per blob. Raw passthrough (pre-framed bytes, no compression). `copy_file_range` for zero-copy splicing (buffered mode only). Three backends: buffered, O_DIRECT, io_uring.

**API surface:**
- `pub enum Compression` (None / Zlib(0-9) / Zstd(-7..22)) + `ParseCompressionError` + `impl FromStr`
- `pub struct PbfWriter<W: Write>` (+ `new`, `to_path`, `to_path_direct`, `to_path_uring`)
- `pub fn write_header / write_primitive_block / write_raw / write_raw_chunks / flush / into_inner`
- `pub(crate) fn write_primitive_block_owned / _owned_pooled`
- `pub struct Metadata<'a>` (re-exported from block_builder)
- `pub(crate) struct FramedBlobParts / OutputChunk` + `pub(crate) trait OutputSink`

**Inbound deps:** lib.rs; commands/getid.rs.

**Outbound deps:** crate::blob_index, crate::write::file_writer, crate::write::metrics, crate::reorder_buffer, crate::write::uring_writer, flate2, zstd, rayon, protohoggr.

---

## `src/geocode_index/`

### `src/geocode_index/mod.rs` (17 lines)

**Purpose:** Module root. Declares `format` (always); `reader` (feature `geocode-reader`); `builder` (feature `commands`).

**API surface:** Module declarations only.

**Inbound deps:** None (root).

**Outbound deps:** None.

---

### `src/geocode_index/format.rs` (766 lines)

**Purpose:** Complete on-disk binary format for the reverse geocoding index. Little-endian record structures: header, S2 geo cells, streets, addresses, interpolation ways, admin polygons, string pool. Manual serialization (no `#[repr(C)]` to avoid padding issues). Ring parsing utilities. On-disk file constants. Roundtrip tests.

**API surface:**
- Constants: `HEADER_MAGIC` ("GIDX"), `FORMAT_VERSION` (v2), `HEADER_SIZE` (128), per-record sizes (`GEO_CELL_SIZE`, `STREET_WAY_SIZE`, `ADDR_POINT_SIZE`, `INTERP_WAY_SIZE`, `ADMIN_CELL_SIZE`, `ADMIN_POLYGON_SIZE`, `NODE_COORD_SIZE`, `SEGMENT_REF_SIZE`), `RING_SENTINEL`, `FILE_*` (file names)
- Structs (with `to_bytes`/`from_bytes`): `Header`, `GeoCell`, `StreetWay`, `AddrPoint`, `InterpWay`, `AdminCell`, `AdminPolygon`, `SegmentRef`, `NodeCoord`
- `fn parse_rings / parse_polygon_rings / read_nul_string`
- `enum FormatError`

**Inbound deps:** geocode_index/reader.rs, geocode_index/builder/{mod, admin, interp, pass1, pass2, pass3}.rs.

**Outbound deps:** None (std only).

---

### `src/geocode_index/reader.rs` (1084 lines)

**Purpose:** Memory-mapped reverse geocoding index reader. Opens directory, mmaps all files, two query interfaces: raw `Candidates` (all hits), ranked `ReverseResult` (nearest per type, smallest admin per level). Binary search + distance filter on geo cells; street lookup; address point extraction; interpolation house number computation; admin polygon retrieval. Two spatial indices (fine + coarse).

**API surface:**
- `struct ReverseResult / AddressMatch / StreetMatch / InterpolationMatch / AdminMatch / Candidates / InterpolationCandidate`
- `impl Candidates::into_result`
- `struct Reader` (Send + Sync) (+ `open`, `query`, `candidates`, `interpolate`, header accessors)

**Inbound deps:** None directly grep'd (public library API exposed to consumers via lib.rs re-export of geocode_index module).

**Outbound deps:** crate::geo, super::format.

---

### `src/geocode_index/builder/mod.rs` (385 lines)

**Purpose:** Module root + main orchestrator for the multi-pass geocode index builder. Coordinates Pass 1 (relations to admin metadata + way IDs), Pass 1.5 (referenced node collection for planet RSS), Pass 2 (fused nodes+ways scan to coords/streets/addresses/interp/admin), Pass 3 (bucketed S2 cell assignment + index writes). Declares submodules and shared config structs. Single entry point `build_geocode_index()`.

**API surface:**
- `struct BuildConfig` / `struct BuildStats`
- `fn build_geocode_index(config) -> Result<BuildStats>`

**Inbound deps:** None directly grep'd (called via `commands::build_geocode_index` dispatch).

**Outbound deps:** crate::ElementReader, super::format.

---

### `src/geocode_index/builder/pass1.rs` (98 lines)

**Purpose:** Pass 1: relation scan for admin boundary collection. Filters by `boundary=administrative` or `boundary=postal_code`, validates admin_level (2-10) or postal mark (11), interns name + ISO3166 country code, collects outer/inner way IDs. Returns admin relation metadata + `IdSetDense` of all referenced way IDs for downstream passes.

**API surface:**
- `struct RawAdminRelation`
- `fn run_pass1() -> (Vec<RawAdminRelation>, IdSetDense)`

**Inbound deps:** None (called by builder/mod.rs).

**Outbound deps:** crate::BlobFilter, crate::Element, crate::ElementReader, crate::MemberId, crate::commands::id_set_dense::IdSetDense, super::strings::StringPool, super::super::format.

---

### `src/geocode_index/builder/pass1_5.rs` (213 lines)

**Purpose:** Pass 1.5: planet-scale memory optimization via referenced node collection. Parallel workers scan way blobs, extract node refs from geocode-relevant ways (street, building, interp, admin members) without full deserialization, populate shared pre-allocated `IdSetDense`. Consolidates with Pass 2a schedule building (one header walk vs two). Pread for zero-copy I/O.

**API surface:**
- `fn run_pass1_5(...)`
- `fn build_pass2_schedules(...)` (consolidated header walk for Pass 1.5 + 2a + max node ID)

**Inbound deps:** None (called by builder/mod.rs).

**Outbound deps:** crate::commands::id_set_dense::IdSetDense, crate::commands::way_scanner::{GeocodeTagLiterals, scan_way_geocode_tagged_refs}, crate::blob::decompress_blob_raw, crate::debug::emit_marker, super::Result.

---

### `src/geocode_index/builder/pass2.rs` (672 lines)

**Purpose:** Pass 2: fused nodes + ways parallel scan. Phase 2a (parallel nodes) decodes node blobs, streams address points to main thread for string interning + write. Phase 2b (parallel ways) decodes ways, classifies by tags (streets, buildings, interp, admin), resolves coordinates from Phase 2a mmap, emits per-blob records. Main thread merges in blob-sequence order, writes street_ways.bin, street_nodes.bin, addr_points.bin, interp_nodes.bin. Mmap outputs feed Pass 3. Honors highway exclusion list.

**API surface:**
- `const EXCLUDED_HIGHWAYS`
- `struct SlimInterpWay / NodeBlobOut / Pass2Output`
- `fn run_pass2(...)`

**Inbound deps:** None (called by builder/mod.rs).

**Outbound deps:** crate::Element, super::strings::StringPool, super::super::format.

---

### `src/geocode_index/builder/admin.rs` (190 lines)

**Purpose:** Admin polygon assembly + on-disk write. Assembles outer + inner rings from way geometry collected in Pass 2, applies Douglas-Peucker simplification, computes area, attaches holes inside each exterior, writes admin_polygons.bin + admin_vertices.bin. Parallel via rayon (no shared state). Coordinates admin cell assignment in Pass 3.

**API surface:**
- `struct AssembledPolygon`
- `fn assemble_admin_polygons / assemble_one_relation / write_admin_data / write_admin_index`

**Inbound deps:** None (called by builder/mod.rs and pass3.rs).

**Outbound deps:** crate::geo (assemble_rings, simplify_ring, signed_area, point_in_ring), super::pass1::RawAdminRelation, super::super::format, super::Result, super::BuildConfig.

---

### `src/geocode_index/builder/interp.rs` (155 lines)

**Purpose:** Interpolation endpoint resolution via mmap. Reads address points from mmap'd addr_points.bin (populated in Pass 2), builds S2 cell spatial index, matches each interpolation way's endpoints against nearby addresses with same street name. Extracts numeric prefix from house number strings. Updates `start_number` / `end_number` in `SlimInterpWay`. Mmap avoids materializing the full address array.

**API surface:**
- `fn parse_house_number / read_addr_point_mmap / read_node_at`
- `fn resolve_interpolation_endpoints_mmap / find_endpoint_house_number_mmap`

**Inbound deps:** None (called by builder/mod.rs and pass3.rs).

**Outbound deps:** super::pass2::SlimInterpWay, super::strings (StringPool, read_string_from_pool), super::super::format.

---

### `src/geocode_index/builder/pass3.rs` (731 lines)

**Purpose:** Pass 3: S2 cell assignment + cell-index write. Two-stage bucketed pipeline to avoid planet-scale RAM accumulation. Stage A scans streets/addresses/interpolations at fine level, computes S2 cells via segment covering, fuses fine + coarse on the fly (parent cells), partitions into 256 buckets by top 8 cell ID bits. Stage B processes one bucket at a time: sort by cell_id, group entries, write geo_cells.bin, street_entries.bin, etc. Admin cells assigned separately via edge-cover + centroid flood-fill for all polygons.

**API surface:**
- `struct AdminCellEntry`
- `fn cover_segment / assign_admin_cells / admin_cells_for_polygon`
- `fn bucketed_cell_assignment_fused / run_stage_b / write_admin_index`

**Inbound deps:** None (called by builder/mod.rs).

**Outbound deps:** super::admin::AssembledPolygon, super::interp (read_addr_point_mmap, read_node_at), super::pass2::SlimInterpWay, super::super::format.

---

### `src/geocode_index/builder/strings.rs` (40 lines)

**Purpose:** Interned string pool for the builder. Flat byte buffer (`data`) + FxHashMap of offsets for dedup. Produces `strings.bin`. Offset 0 reserved for empty. Used by all passes to intern street names, admin names, house numbers, postcodes, country codes.

**API surface:**
- `struct StringPool` (+ `new`, `intern`)
- `fn read_string_from_pool`

**Inbound deps:** None (used by all builder/* passes).

**Outbound deps:** super::super::format::read_nul_string.

---

## Shared infrastructure currently misfiled under `src/commands/`

These eight files are the explicit Stage 1 lift candidates: each is consumed by multiple commands AND/OR by `src/read/` or `src/geocode_index/`.

### `src/commands/id_set_dense.rs` (816 lines)

**Purpose:** Chunked sparse bitset for O(1) membership and ranking of OSM element IDs. Mirrors osmium's IdSetDense (4 MB chunks covering 33M IDs each). Single-threaded (`set` / `get`) and thread-safe atomic (`set_atomic`) APIs. Rank-index for cardinality queries and prefix-sum operations. The fundamental data structure for planet-scale ID set operations across the codebase.

**API surface:**
- `pub struct IdSetDense` (+ `new`, `set`, `set_if_new`, `pre_allocate`, `set_atomic`, `set_atomic_if_new`, `get`, `has_any`, `allocated_chunk_count`, `any_in_range`, `iter`, `merge_from`, `merge`, `build_rank_index`, `rank`, `rank_if_set`, `resolve`, `count_below`, `count_in_range`, `drop_rank_index`, `total_count`)

**Inbound deps:** commands/altw/{stage1, stage2, stage4, mod, relation_scan}.rs, commands/extract/{common, multi, simple, smart}.rs, commands/add_locations_to_ways.rs, commands/check_refs.rs, commands/getid.rs, commands/renumber_external/{mod, pass1, stage2, wire_rewrite, relations}.rs, commands/tags_filter.rs, commands/verify_ids.rs, geocode_index/builder/{pass1, pass1_5, pass2}.rs, read/indexed.rs.

**Outbound deps:** None (self-contained, std only).

---

### `src/commands/tag_expr.rs` (284 lines)

**Purpose:** Shared tag expression parser and matcher for filtering OSM elements by key/value patterns. Five matcher variants (key-only, key-prefix wildcard, exact value, multi-value union, negation). Optional element type prefix (n/w/r). Reads expressions from files with comment support.

**API surface:**
- `pub enum TagMatcher` (KeyOnly, KeyPrefix, ExactValue, MultiValue, NotValue)
- `pub struct Expression` (type filter + matcher)
- `pub fn read_expressions_file(path) -> Result<Vec<String>>`
- `pub(crate) fn parse_expression / parse_expressions / tag_matches`

**Inbound deps:** commands/tags_count.rs, commands/tags_filter.rs, commands/tags_filter_osc.rs.

**Outbound deps:** crate::commands (TypeFilter from mod.rs).

---

### `src/commands/external_radix.rs` (76 lines)

**Purpose:** Shared infrastructure for 256-bucket radix-partitioned external joins. Managed scratch directory with auto-cleanup on drop, bucket count and buffer size constants, optional POSIX_FADV_DONTNEED page cache eviction. Extracted from external_join during the renumber planet refactor.

**API surface:**
- `pub(crate) const NUM_BUCKETS: usize` (256)
- `pub(crate) const BUCKET_BUF_SIZE: usize` (256 KB)
- `pub(crate) struct ScratchDir` (+ `new`, `bucket_path`, `file_path`)
- `pub(crate) fn advise_dontneed_file(file)` (Linux)

**Inbound deps:** commands/altw/{stage1, stage2, stage3, mod}.rs.

**Outbound deps:** None (self-contained, std only).

---

### `src/commands/node_scanner.rs` (110 lines)

**Purpose:** Wire-format DenseNodes scanner. Extracts (id, lat, lon) tuples directly from decompressed PBF blocks without constructing `PrimitiveBlock`. Bypasses string table parsing and group allocation, eliminating cross-thread accumulation problems. Used by external join stage 2, ALTW dense/sparse pass 1, geocode builder for node index construction.

**API surface:**
- `pub(crate) struct NodeTuple { id: i64, lat: i32, lon: i32 }`
- `pub(crate) fn extract_node_tuples(decompressed, out, group_starts) -> Result<()>`

**Inbound deps:** commands/add_locations_to_ways.rs, commands/extract/simple.rs, commands/altw/stage2.rs, commands/merge/node_locations.rs, read/pipeline.rs.

**Outbound deps:** crate::read::wire (Cursor, WireDenseNodes, PackedSint64Iter, wire constants).

---

### `src/commands/way_scanner.rs` (392 lines)

**Purpose:** Wire-format way scanner. Extracts way IDs and node ref lists from PBF blobs without full PrimitiveBlock construction. Specialized geocode-tagged variant for street/building-addr/interpolation classification used by address indexing. Used by ALTW pass 0, geocode builder pass 1.5, and location merge.

**API surface:**
- `pub(crate) struct WayGeocodeFlags { is_street, is_building_addr, is_interp }`
- `pub(crate) struct GeocodeTagLiterals<'a>` (+ `const fn standard()`)
- `pub(crate) fn scan_way_refs(decompressed, refs_buf, group_starts, callback)`
- `pub(crate) fn scan_way_geocode_tagged_refs(decompressed, literals, refs_buf, group_starts, callback)`

**Inbound deps:** commands/add_locations_to_ways.rs, commands/extract/simple.rs, commands/altw/stage1.rs, geocode_index/builder/pass1_5.rs.

**Outbound deps:** crate::read::wire (Cursor, PackedSint64Iter, PackedUint32Iter, wire constants).

---

### `src/commands/elements_pbf.rs` (316 lines)

**Purpose:** PBF-oriented owned element types for round-trip decode-process-encode. Owned `Node`, `Way`, `Relation` with `Vec`-based storage for transient allocations in sort, merge_pbf, time_filter. Read functions converting from parsed elements; write functions dispatching to `BlockBuilder`.

**API surface:**
- `pub(crate) struct OwnedMetadata / OwnedNode / OwnedWay / OwnedRelation`
- `pub(crate) enum OwnedElement`
- `pub(crate) fn read_dense_node / read_node / read_way / read_relation`
- `pub(crate) fn write_single_node / write_single_way / write_single_relation`
- Ord/Eq impls for ID-based sorting (version tiebreaker)

**Inbound deps:** commands/sort.rs, commands/merge_pbf.rs, commands/time_filter.rs.

**Outbound deps:** crate::block_builder (BlockBuilder, MemberData, Metadata), crate::file_writer, crate::writer (PbfWriter), crate::commands::elements_xml (OwnedMember re-export).

---

### `src/commands/elements_xml.rs` (420 lines)

**Purpose:** XML-oriented owned element types for OSC change-set operations. Metadata fields are `String` (direct XML attribute output). XML serialization (owned + borrowed paths to avoid cloning) for derive_changes, diff, merge_changes, tags_filter_osc. Coordinate conversion + formatting.

**API surface:**
- `pub(crate) struct OwnedMetadata / OwnedNode / OwnedWay / OwnedMember / OwnedRelation`
- `pub(crate) fn nodes_equal / ways_equal / members_equal / relations_equal`
- `pub(crate) fn from_decimicro / format_coord`
- `pub(crate) fn write_node_xml / write_way_xml / write_relation_xml / write_delete_xml / write_element_xml` (zero-clone borrowed path)

**Inbound deps:** commands/diff.rs, commands/derive_changes.rs, commands/merge_changes.rs, commands/tags_filter_osc.rs, commands/elements_pbf.rs (OwnedMember import), commands/stream_merge.rs (equality functions for MergeJoinElement).

**Outbound deps:** crate::MemberId.

---

### `src/commands/stream_merge.rs` (920 lines)

**Purpose:** Streaming element cursor and merge-join infrastructure for sorted PBF operations. `StreamingBlocks` wraps a pipelined reader with transparent stashing of wrong-type blocks between phases. `merge_join_phase` is a generic two-pointer merge-join over typed element streams used by diff and derive_changes. Type-agnostic `MergeJoinElement` trait with built-in implementations for nodes / ways / relations.

**API surface:**
- `pub(crate) struct StreamingBlocks` (+ `new_sequential(path, direct_io)`)
- `pub(crate) enum MergeJoinAction<'a, T>` (OldOnly / NewOnly / Modified / Equal)
- `pub(crate) trait MergeJoinElement` (id, is_block_type, equal, convert)
- `pub(crate) fn merge_join_phase<T>(...)`
- Block predicates: `is_node_block / is_way_block / is_relation_block`
- Conversion: `convert_node / convert_way / convert_relation`
- Helpers: `element_id / element_version`

**Inbound deps:** commands/derive_changes.rs, commands/diff.rs.

**Outbound deps:** crate::blob_index (ElemKind), crate (BlockType, Element, PrimitiveBlock), crate::commands::elements_xml.

---

## Notes for the categorization step

Observations from the inventory worth raising before we draw package boundaries:

1. **`crate::commands::id_set_dense` is the single most widely consumed shared module** in the inventory: 22 inbound dependencies spanning altw/, extract/, renumber_external/, geocode_index/builder/, and `read/indexed.rs`. Genuinely a top-level data structure, not command-specific.
2. **`elements_pbf` and `elements_xml` cross-reference each other** (`elements_pbf` re-exports `OwnedMember` from `elements_xml`). They are siblings of one logical "owned element representation" concern, even though the formats they serve are different (PBF vs OSC XML).
3. **`stream_merge` depends on `elements_xml`** for equality functions used in `MergeJoinElement` impls. Bundling concern: stream_merge cannot land in a different package from elements_xml without untangling that.
4. **`node_scanner` and `way_scanner` are siblings** of one "lightweight wire scanner" concern. Both consumed by ALTW, extract, geocode_index/builder.
5. **`tag_expr` has a back-edge to `crate::commands::TypeFilter`** (defined in `commands/mod.rs`). Lifting `tag_expr` cleanly requires either lifting `TypeFilter` too, or splitting it.
6. **`external_radix` is altw-only** in current consumption (4 inbound, all under `commands/altw/`). It is shared infra by intent, but not by current usage. Lifting it to a top-level package may not be justified yet; could stay inside altw/ until a second consumer appears.
7. **`debug.rs` has only two grep hits** but is almost certainly used pervasively via `crate::debug::emit_*` calls that the subagent did not surface as `use` lines (functions called via path expression rather than imported by name). The inventory's inbound list under-reports its real reach.
8. **`indexed.rs` reaches into `crate::commands::id_set_dense`** today, so even pure-`read/` code depends on the misfiled infra. Lifting `id_set_dense` is a precondition for `read/` to be self-contained.
9. **`geocode_index/builder/` heavily depends on misfiled infra** (id_set_dense, way_scanner, debug). The geocode subsystem's clean boundaries are blocked on Stage 1.
10. **Two `OwnedMetadata` and two `OwnedNode/Way/Relation` types** exist (one in elements_pbf, one in elements_xml). Worth deciding during categorization whether to keep them separate (different fields, different downstream needs) or unify.
