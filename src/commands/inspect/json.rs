//! JSON serialization of `InspectReport`. Gated on the `commands` feature to
//! avoid pulling in `serde_json` for library-only consumers.

#![cfg(feature = "commands")]

use super::format::format_timestamp;
use super::types::{
    BlockInfo, ExtendedStats, InspectReport, LocationStats, MetadataCoverage, ScanState,
    TypeIdRange, TypeStats, anomaly_blocks, is_standard_ordering,
};

impl InspectReport {
    /// Serialize the inspect report to a JSON value.
    ///
    /// `block_limit`: `None` = no `blocks_detail` field, `Some(0)` = full listing,
    /// `Some(N)` = first N + last N blocks.
    #[cfg(feature = "commands")]
    pub fn to_json(&self, block_limit: Option<usize>) -> serde_json::Value {
        self.to_json_filtered(block_limit, false)
    }

    /// Serialize the inspect report to JSON with optional anomalies-only block detail.
    #[cfg(feature = "commands")]
    pub fn to_json_filtered(
        &self,
        block_limit: Option<usize>,
        anomalies_only: bool,
    ) -> serde_json::Value {
        let hm = &self.header_meta;

        let bbox = hm.bbox.map(|(left, bottom, right, top)| {
            serde_json::json!({ "left": left, "bottom": bottom, "right": right, "top": top })
        });

        let header = serde_json::json!({
            "writing_program": hm.writing_program,
            "required_features": hm.required_features,
            "optional_features": hm.optional_features,
            "bbox": bbox,
            "replication": {
                "sequence": hm.replication_sequence,
                "timestamp": hm.replication_timestamp,
                "url": hm.replication_url,
            },
        });

        let sequence: Vec<&str> = self
            .accum
            .segments
            .iter()
            .map(|s| s.kind.short_label())
            .collect();

        let mut json = serde_json::json!({
            "schema_version": 1,
            "file": self.file_name,
            "file_size": self.file_size,
            "header": header,
            "indexed": self.is_indexed,
            "blocks": {
                "total": self.total_blocks,
                "nodes": type_stats_json(&self.accum.node_type),
                "ways": type_stats_json(&self.accum.way_type),
                "relations": type_stats_json(&self.accum.relation_type),
                "mixed": type_stats_json(&self.accum.mixed_type),
            },
            "elements": {
                "nodes": self.state.node_count,
                "tagged_nodes": self.state.tagged_node_count,
                "ways": self.state.way_count,
                "relations": self.state.relation_count,
                "total": self.state.node_count + self.state.way_count + self.state.relation_count,
            },
            "ordering": {
                "sequence": sequence,
                "standard": is_standard_ordering(&self.accum.segments),
            },
            "id_ranges": id_ranges_json(&self.state),
            "anomalies_only": anomalies_only,
            "blocks_detail": blocks_detail_json(block_limit, &self.accum.block_infos, anomalies_only),
            "locations": locations_json(&self.state.loc_stats),
        });

        if let Some(ref ext) = self.state.extended {
            json["data"] = extended_json(ext);
            json["metadata"] = metadata_json(&ext.metadata);
        }

        json
    }
}

#[cfg(feature = "commands")]
fn type_stats_json(ts: &TypeStats) -> serde_json::Value {
    serde_json::json!({
        "count": ts.block_count,
        "compressed_bytes": ts.frame_bytes,
        "elements": ts.element_count,
    })
}

#[cfg(feature = "commands")]
fn id_range_json(r: &TypeIdRange) -> serde_json::Value {
    if r.has_data() {
        serde_json::json!({ "min": r.min_id, "max": r.max_id, "monotonic": r.monotonic, "count": r.count })
    } else {
        serde_json::Value::Null
    }
}

#[cfg(feature = "commands")]
fn id_ranges_json(state: &ScanState) -> serde_json::Value {
    match (&state.node_ids, &state.way_ids, &state.relation_ids) {
        (Some(n), Some(w), Some(r)) => serde_json::json!({
            "nodes": id_range_json(n),
            "ways": id_range_json(w),
            "relations": id_range_json(r),
        }),
        _ => serde_json::Value::Null,
    }
}

#[allow(clippy::cast_precision_loss)]
#[cfg(feature = "commands")]
fn extended_json(ext: &ExtendedStats) -> serde_json::Value {
    let bbox = if ext.data_bbox.has_data() {
        let bb = &ext.data_bbox;
        serde_json::json!([
            bb.min_lon as f64 * 1e-9,
            bb.min_lat as f64 * 1e-9,
            bb.max_lon as f64 * 1e-9,
            bb.max_lat as f64 * 1e-9
        ])
    } else {
        serde_json::Value::Null
    };

    let timestamp = if ext.has_timestamps() {
        serde_json::json!({
            "first": format_timestamp(ext.min_timestamp),
            "last": format_timestamp(ext.max_timestamp),
        })
    } else {
        serde_json::Value::Null
    };

    serde_json::json!({
        "bbox": bbox,
        "timestamp": timestamp,
        "objects_ordered": ext.objects_ordered,
    })
}

#[cfg(feature = "commands")]
fn metadata_json(m: &MetadataCoverage) -> serde_json::Value {
    serde_json::json!({
        "all_objects": {
            "version": m.all_have(m.has_version),
            "timestamp": m.all_have(m.has_timestamp),
            "changeset": m.all_have(m.has_changeset),
            "uid": m.all_have(m.has_uid),
            "user": m.all_have(m.has_user),
        },
        "some_objects": {
            "version": m.some_have(m.has_version),
            "timestamp": m.some_have(m.has_timestamp),
            "changeset": m.some_have(m.has_changeset),
            "uid": m.some_have(m.has_uid),
            "user": m.some_have(m.has_user),
        },
    })
}

#[cfg(feature = "commands")]
fn blocks_detail_json(
    block_limit: Option<usize>,
    block_infos: &Option<Vec<BlockInfo>>,
    anomalies_only: bool,
) -> serde_json::Value {
    let (Some(limit), Some(infos)) = (block_limit, block_infos) else {
        return serde_json::Value::Null;
    };
    if anomalies_only {
        let selected = anomaly_blocks(infos);
        let arr: Vec<serde_json::Value> = selected
            .iter()
            .map(|(info, reason)| {
                serde_json::json!({
                    "number": info.number,
                    "type": info.kind.short_label(),
                    "elements": info.elements,
                    "compressed_bytes": info.compressed,
                    "raw_bytes": info.raw,
                    "anomaly": reason,
                })
            })
            .collect();
        return serde_json::Value::Array(arr);
    }
    let selected: Vec<&BlockInfo> = infos.iter().collect();
    let truncate = limit > 0 && limit * 2 < selected.len();
    let iter: Box<dyn Iterator<Item = &BlockInfo>> = if truncate {
        Box::new(
            selected[..limit]
                .iter()
                .copied()
                .chain(selected[selected.len() - limit..].iter().copied()),
        )
    } else {
        Box::new(selected.iter().copied())
    };
    let arr: Vec<serde_json::Value> = iter
        .map(|info| {
            serde_json::json!({
                "number": info.number,
                "type": info.kind.short_label(),
                "elements": info.elements,
                "compressed_bytes": info.compressed,
                "raw_bytes": info.raw,
            })
        })
        .collect();
    serde_json::Value::Array(arr)
}

#[cfg(feature = "commands")]
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn locations_json(loc_stats: &Option<LocationStats>) -> serde_json::Value {
    let Some(stats) = loc_stats else {
        return serde_json::Value::Null;
    };
    let coords_per_way = if stats.coord_counts.is_empty() {
        serde_json::Value::Null
    } else {
        let mut sorted = stats.coord_counts.clone();
        sorted.sort_unstable();
        let len = sorted.len();
        let p99_idx = ((len as f64 - 1.0) * 0.99) as usize;
        serde_json::json!({
            "min": sorted[0],
            "max": sorted[len - 1],
            "median": sorted[len / 2],
            "p99": sorted[p99_idx.min(len - 1)],
        })
    };
    serde_json::json!({
        "with_locations": stats.with_locations,
        "without_locations": stats.without_locations,
        "coords_per_way": coords_per_way,
    })
}
