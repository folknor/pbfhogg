//! Complete-ways extraction strategy (two passes).

use crate::cat::CleanAttrs;
use crate::read::input::PinnedInput;
use crate::write::WriteOptions;

use super::super::{HeaderOverrides, Result, writer_from_header};

use super::common::{
    BboxInt, ExtractPass2IdSets, extract_block_pass2, pread_write_pass,
    pread_write_pass_with_schedule,
};
use super::smart::{CompleteRelationHandler, collect_pass1_generic, write_schedule_for};
use super::{ExtractStats, Region};

#[cfg_attr(feature = "hotpath", hotpath::measure)]
#[allow(clippy::too_many_arguments)]
pub(super) fn extract_complete_ways(
    input: &PinnedInput,
    output: &crate::output::BoundOutput,
    region: &Region,
    set_bounds: bool,
    clean: &CleanAttrs,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Entered<'_>,
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
    host.invocation.marker("COMPLETE_PASS1_START");
    let bbox_int = BboxInt::from_bbox(region.bbox());
    let mut handler = CompleteRelationHandler;
    let mut result =
        collect_pass1_generic(input, region, &bbox_int, direct_io, &mut handler, host)?;
    host.invocation.marker("COMPLETE_PASS1_END");

    // --- Pass 2: Write matching elements via pread-from-workers ---
    host.invocation.marker("COMPLETE_PASS2_START");
    host.invocation.marker("COMPLETE_PASS2_SETUP_START");

    let mut header_reader = crate::blob::BlobReader::open_in(input, direct_io, host)?;
    let header_blob = header_reader.leading_header()?;
    let header = header_blob.to_headerblock()?;
    drop(header_reader);
    let coords = crate::owned::WayCoords::of_input(&header);
    let bbox = region.bbox();
    let mut writer = writer_from_header(
        output,
        compression,
        &header,
        &super::output_facts(set_bounds, bbox, coords),
        overrides,
        backend,
        host,
    )?;

    let pass1_blob_schedule = match &result.schedules {
        Some(schedules) => Some(write_schedule_for(
            schedules,
            &[&result.bbox_node_ids, &result.outside_way_node_ids],
            &[&result.matched_way_ids],
            &[&result.matched_relation_ids],
            host,
        )?),
        None => None,
    };
    result.schedules = None;

    let ids = ExtractPass2IdSets {
        bbox_node_ids: &result.bbox_node_ids,
        outside_way_node_ids: &result.outside_way_node_ids,
        matched_way_ids: &result.matched_way_ids,
        matched_relation_ids: &result.matched_relation_ids,
    };

    host.invocation.marker("COMPLETE_PASS2_SETUP_END");
    host.invocation.marker("COMPLETE_PASS2_WRITE_START");
    // The pass 1 schedule, cut by span, on sorted input; the unsorted
    // fallback builds a full one.
    if let Some(pass1_blob_schedule) = pass1_blob_schedule {
        pread_write_pass_with_schedule(
            input,
            &pass1_blob_schedule,
            &mut writer,
            &mut stats,
            |block, bb, output_blocks| {
                extract_block_pass2(block, &ids, clean, coords, None, bb, output_blocks)
            },
            host,
        )?;
    } else {
        pread_write_pass(
            input,
            &mut writer,
            &mut stats,
            |block, bb, output_blocks| {
                extract_block_pass2(block, &ids, clean, coords, None, bb, output_blocks)
            },
            host,
        )?;
    }
    host.invocation.marker("COMPLETE_PASS2_WRITE_END");

    host.invocation.marker("COMPLETE_PASS2_END");
    Ok(stats)
}
