//! Raw group passthrough: assemble PrimitiveBlock protobuf from raw components.
//!
//! For groups where all elements are selected, copy the raw protobuf group bytes
//! instead of decoding + re-encoding via BlockBuilder. The StringTable is copied
//! whole. Only partial-match groups need full decode + re-encode.
//!
//! **Status:** primitives are scaffolding (`#[allow(dead_code)]`) with no live
//! consumer. Blob-level passthrough in extract / cat / getid handles the
//! 90 %+-interior cases and is the right granularity when it fires.
//!
//! **Mixing raw and re-encoded groups in one output blob** is the real design
//! question any future consumer has to answer. Raw groups carry the original
//! string-table indices; BlockBuilder issues fresh indices. Two viable shapes:
//!
//! 1. *String-table-aligned re-encode*: copy the original StringTable whole,
//!    re-encode partial groups against those existing indices instead of
//!    BlockBuilder's own table. Requires a different encode path on the
//!    write side; preserves one output blob per input blob.
//! 2. *Split output*: emit raw-only groups and re-encoded groups as separate
//!    output blobs. Simpler (both paths are already in the writer), doubles
//!    boundary-blob count - acceptable if boundary blobs are a small
//!    fraction of the output.
//!
//! **Measure first.** The same class of shadow-counter measurement - "how
//! many blobs would actually qualify under this gate, on a real workload" -
//! is the prerequisite for building either of the per-group shapes above.
//! Two such measurements have already disproven blob-level gates in
//! different commands at planet scale:
//!
//! - **tags-filter** (2026-04-18, UUID `8c786794`, `w/highway=primary` on
//!   planet): 0 / 50,364 pass-2 blobs qualified for "all-elements-included"
//!   blob passthrough. Load-bearing pin in
//!   `src/commands/tags_filter/mod.rs` pass-2 worker.
//! - **multi-extract** (2026-04-20, UUID `dad573cb`, planet 5-region
//!   `--config --simple`): 0 / 32,835 node blobs qualified for
//!   all-N-contained or partial-contained passthrough. Load-bearing pin
//!   in `src/commands/extract/multi.rs::try_extract_multi_single_pass`.
//!
//! Both measurements are structural, not workload-specific: PBF blobs are
//! ID-sorted and OSM IDs are chronological rather than geographic or
//! tag-coherent, so an 8,000-element blob scatters across the planet in
//! both dimensions. Per-element match rates stay low, per-blob
//! "all-elements-qualify" rates are effectively zero. Any new consumer of
//! this scaffolding should shadow-count first and prove the qualifying
//! fraction is non-trivial on a real workload before building.

use protohoggr::{encode_bytes_field_always, encode_varint};

/// Assemble a PrimitiveBlock protobuf from raw StringTable bytes,
/// raw/re-encoded group bytes, and scalar fields.
///
/// The output is the serialized PrimitiveBlock message (not framed - the caller
/// wraps it via `PbfWriter::write_primitive_block` or similar).
///
/// # Arguments
/// - `stringtable`: raw StringTable protobuf bytes (field 1 submessage content)
/// - `groups`: list of raw PrimitiveGroup protobuf bytes (field 2 submessage content each)
/// - `granularity`: field 17 (default 100)
/// - `lat_offset`: field 19 (default 0)
/// - `lon_offset`: field 20 (default 0)
/// - `date_granularity`: field 18 (default 1000)
#[allow(clippy::cast_sign_loss)]
#[allow(dead_code)] // Scaffolding for future per-group raw passthrough
pub(crate) fn frame_raw_block(
    stringtable: &[u8],
    groups: &[&[u8]],
    granularity: i32,
    lat_offset: i64,
    lon_offset: i64,
    date_granularity: i32,
    out: &mut Vec<u8>,
) {
    out.clear();

    // Field 1: StringTable (len-delimited)
    encode_bytes_field_always(out, 1, stringtable);

    // Field 2: PrimitiveGroup (repeated, len-delimited)
    for group in groups {
        encode_bytes_field_always(out, 2, group);
    }

    // Field 17: granularity (varint, default 100)
    if granularity != 100 {
        encode_varint(out, (17 << 3) as u64); // field 17, wire type 0
        encode_varint(out, granularity as u64);
    }

    // Field 18: date_granularity (varint, default 1000)
    if date_granularity != 1000 {
        encode_varint(out, (18 << 3) as u64);
        encode_varint(out, date_granularity as u64);
    }

    // Field 19: lat_offset (varint, default 0)
    if lat_offset != 0 {
        encode_varint(out, (19 << 3) as u64);
        encode_varint(out, lat_offset as u64);
    }

    // Field 20: lon_offset (varint, default 0)
    if lon_offset != 0 {
        encode_varint(out, (20 << 3) as u64);
        encode_varint(out, lon_offset as u64);
    }
}
