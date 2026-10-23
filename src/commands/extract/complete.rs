//! Complete-ways extraction strategy (two passes).

use std::path::Path;

use crate::cat::CleanAttrs;
use crate::write::WriteOptions;

use super::super::{HeaderOverrides, Result, writer_from_header};

use super::common::{
    BboxInt, ExtractPass2IdSets, extract_block_pass2, pread_write_pass,
    pread_write_pass_with_schedule,
};
use super::smart::{CompleteRelationHandler, collect_pass1_generic};
use super::{ExtractStats, Region};

#[cfg_attr(feature = "hotpath", hotpath::measure)]
#[allow(clippy::too_many_arguments)]
pub(super) fn extract_complete_ways(
    input: &Path,
    output: &crate::output::Output,
    region: &Region,
    set_bounds: bool,
    clean: &CleanAttrs,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
) -> Result<ExtractStats> {
    let WriteOptions {
        compression,
        backend,
        direct_reads: direct_io,
        ..
    } = *write;
    let mut stats = ExtractStats {
        nodes_in_bbox: 0,
        nodes_from_ways: 0,
        nodes_from_relations: 0,
        ways_written: 0,
        ways_from_relations: 0,
        relations_written: 0,
        strategy: "complete_ways",
    };

    // --- Pass 1: Collect matches ---
    crate::debug::emit_marker("COMPLETE_PASS1_START");
    let bbox_int = BboxInt::from_bbox(region.bbox());
    let mut handler = CompleteRelationHandler;
    let mut result =
        collect_pass1_generic(input, region, &bbox_int, direct_io, &mut handler, host)?;
    crate::debug::emit_marker("COMPLETE_PASS1_END");

    // --- Pass 2: Write matching elements via pread-from-workers ---
    crate::debug::emit_marker("COMPLETE_PASS2_START");
    crate::debug::emit_marker("COMPLETE_PASS2_SETUP_START");

    let mut header_reader = crate::blob::BlobReader::open_in(input, direct_io, host)?;
    let header_blob = header_reader.leading_header()?;
    let header = header_blob.to_headerblock()?;
    drop(header_reader);
    let bbox = region.bbox();
    let mut writer = writer_from_header(
        output,
        compression,
        &header,
        &super::output_facts(set_bounds, bbox),
        overrides,
        backend,
        host,
    )?;

    // Take the pre-built blob schedule BEFORE creating `ids`, since `ids`
    // holds immutable borrows of `result` and we need a brief mutable borrow
    // here to mem::take the schedule.
    let pass1_blob_schedule = std::mem::take(&mut result.pass3_blob_schedule);

    let ids = ExtractPass2IdSets {
        bbox_node_ids: &result.bbox_node_ids,
        all_way_node_ids: &result.all_way_node_ids,
        matched_way_ids: &result.matched_way_ids,
        matched_relation_ids: &result.matched_relation_ids,
    };

    crate::debug::emit_marker("COMPLETE_PASS2_SETUP_END");
    crate::debug::emit_marker("COMPLETE_PASS2_WRITE_START");
    // Reuse PASS1's pre-built blob schedule if available, falling back to
    // build_blob_schedule for the unsorted-fallback path. Avoids the second
    // post-PASS1 header scan and its cold-arena-page residency cascade.
    if pass1_blob_schedule.is_empty() {
        pread_write_pass(
            input,
            &mut writer,
            &mut stats,
            |block, bb, output_blocks| {
                extract_block_pass2(block, &ids, clean, None, bb, output_blocks)
            },
            host,
        )?;
    } else {
        pread_write_pass_with_schedule(
            input,
            &pass1_blob_schedule,
            &mut writer,
            &mut stats,
            |block, bb, output_blocks| {
                extract_block_pass2(block, &ids, clean, None, bb, output_blocks)
            },
            host,
        )?;
    }
    crate::debug::emit_marker("COMPLETE_PASS2_WRITE_END");

    crate::debug::emit_marker("COMPLETE_PASS2_END");
    Ok(stats)
}
