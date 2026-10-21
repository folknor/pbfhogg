//! Human-readable report output: `InspectReport::write_report` family plus
//! `get_value` dot-path lookup.
//!
//! The report is rendered into a caller-supplied writer rather than straight
//! to the process's stdout. A library does not own its host's stdout, and
//! the CLI needs every write error back as a value: `println!` panics when
//! the reader of a pipe has gone away (`pbfhogg inspect x | head -1`), and a
//! returned `io::Error` is what lets the binary end quietly instead.

use std::io::{self, Write};

use super::format::{format_number, format_number_signed, format_size, format_timestamp, yes_no};
use super::types::{
    BlockInfo, BlockKind, ExtendedStats, InspectReport, LocationStats, MetadataCoverage,
    TypeIdRange, anomaly_blocks, is_standard_ordering,
};

impl InspectReport {
    /// Write the full inspect report to `out`.
    ///
    /// `block_limit`: `None` = no block detail, `Some(0)` = distribution stats
    /// + full listing, `Some(N)` = distribution stats + first/last N blocks.
    ///
    /// # Errors
    ///
    /// Returns the first error `out` reports.
    pub fn write_report<W: Write>(
        &mut self,
        out: &mut W,
        block_limit: Option<usize>,
    ) -> io::Result<()> {
        self.write_report_filtered(out, block_limit, false)
    }

    /// Write the inspect report to `out`, with optional anomalies-only block
    /// detail.
    ///
    /// # Errors
    ///
    /// Returns the first error `out` reports.
    pub fn write_report_filtered<W: Write>(
        &mut self,
        out: &mut W,
        block_limit: Option<usize>,
        anomalies_only: bool,
    ) -> io::Result<()> {
        self.write_header(out)?;
        writeln!(out)?;
        self.write_blocks_summary(out)?;
        writeln!(out)?;
        self.write_elements(out)?;
        writeln!(out)?;
        self.write_ordering(out)?;

        if let Some(ref infos) = self.accum.block_infos {
            writeln!(out)?;
            Self::write_block_distribution(out, infos)?;
            writeln!(out)?;
            if anomalies_only {
                let selected = anomaly_blocks(infos);
                writeln!(
                    out,
                    "Block anomalies ({} of {} - <50% or >150% of per-type median):",
                    selected.len(),
                    infos.len()
                )?;
                Self::write_block_table_with_reason(out, &selected)?;
            } else {
                let selected: Vec<&BlockInfo> = infos.iter().collect();
                let limit = block_limit.unwrap_or(0);
                Self::write_block_table_refs(out, &selected, limit)?;
            }
        }
        if let Some((n, w, r)) = self.id_range_tuple() {
            writeln!(out)?;
            Self::write_id_ranges(out, n, w, r)?;
        }
        if let Some(ref ext) = self.state.extended {
            writeln!(out)?;
            Self::write_extended(out, ext)?;
        }
        if let Some(ref mut stats) = self.state.loc_stats {
            writeln!(out)?;
            Self::write_locations(out, stats)?;
        }
        Ok(())
    }

    fn write_header(&self, out: &mut impl Write) -> io::Result<()> {
        writeln!(
            out,
            "File:     {} ({})",
            self.file_name,
            format_size(self.file_size)
        )?;
        if let Some(ref prog) = self.header_meta.writing_program {
            writeln!(out, "Program:  {prog}")?;
        }

        // Combine features, skip boilerplate (OsmSchema-V0.6, DenseNodes)
        let features: Vec<&str> = self
            .header_meta
            .required_features
            .iter()
            .chain(self.header_meta.optional_features.iter())
            .map(String::as_str)
            .filter(|f| *f != "OsmSchema-V0.6" && *f != "DenseNodes")
            .collect();
        if !features.is_empty() {
            writeln!(out, "Features: {}", features.join(", "))?;
        }

        if let Some((left, bottom, right, top)) = self.header_meta.bbox {
            writeln!(out, "Bbox:     {left},{bottom},{right},{top}")?;
        }

        let hm = &self.header_meta;
        if hm.replication_sequence.is_some() || hm.replication_timestamp.is_some() {
            let mut parts = Vec::new();
            if let Some(seq) = hm.replication_sequence {
                parts.push(format!("seq {seq}"));
            }
            if let Some(ts) = hm.replication_timestamp {
                parts.push(format!("timestamp {ts}"));
            }
            if let Some(ref url) = hm.replication_url {
                parts.push(url.clone());
            }
            writeln!(out, "Repl:     {}", parts.join(", "))?;
        }

        writeln!(
            out,
            "Indexed:  {}",
            if self.is_indexed { "yes" } else { "no" }
        )
    }

    fn write_blocks_summary(&self, out: &mut impl Write) -> io::Result<()> {
        writeln!(out, "Blocks:   {} total", self.total_blocks)?;
        for (label, stats) in [
            (BlockKind::Nodes.label(), &self.accum.node_type),
            (BlockKind::Ways.label(), &self.accum.way_type),
            (BlockKind::Relations.label(), &self.accum.relation_type),
            (BlockKind::Mixed.label(), &self.accum.mixed_type),
        ] {
            if stats.block_count > 0 {
                writeln!(
                    out,
                    "  {:13}{:>6}  ({} compressed)",
                    label,
                    stats.block_count,
                    format_size(stats.frame_bytes)
                )?;
            }
        }
        Ok(())
    }

    fn write_elements(&self, out: &mut impl Write) -> io::Result<()> {
        let total = self.state.node_count + self.state.way_count + self.state.relation_count;
        writeln!(out, "Elements: {} total", format_number(total))?;

        if self.state.tagged_node_count > 0 {
            writeln!(
                out,
                "  {:13}{}  ({} tagged)",
                "Nodes:",
                format_number(self.state.node_count),
                format_number(self.state.tagged_node_count)
            )?;
        } else {
            writeln!(
                out,
                "  {:13}{}",
                "Nodes:",
                format_number(self.state.node_count)
            )?;
        }
        writeln!(
            out,
            "  {:13}{}",
            "Ways:",
            format_number(self.state.way_count)
        )?;
        writeln!(
            out,
            "  {:13}{}",
            "Relations:",
            format_number(self.state.relation_count)
        )
    }

    fn write_ordering(&self, out: &mut impl Write) -> io::Result<()> {
        if self.accum.segments.is_empty() {
            return writeln!(out, "Ordering: (empty file)");
        }

        let labels: Vec<&str> = self
            .accum
            .segments
            .iter()
            .map(|s| s.kind.short_label())
            .collect();
        let sequence = labels.join(" \u{2192} ");

        if is_standard_ordering(&self.accum.segments) {
            writeln!(out, "Ordering: {sequence} (strict)")?;
        } else {
            writeln!(out, "Ordering: {sequence} (NON-STANDARD)")?;
            let ranges: Vec<String> = self
                .accum
                .segments
                .iter()
                .map(|s| {
                    if s.first_block == s.last_block {
                        format!("block {}", s.first_block)
                    } else {
                        format!("blocks {}-{}", s.first_block, s.last_block)
                    }
                })
                .collect();
            writeln!(out, "          {}", ranges.join("  "))?;
        }
        Ok(())
    }

    /// Write per-type distribution stats (min/max/median/p99) for block
    /// element counts and compressed sizes.
    fn write_block_distribution(out: &mut impl Write, infos: &[BlockInfo]) -> io::Result<()> {
        writeln!(out, "Block distribution:")?;
        for kind in [
            BlockKind::Nodes,
            BlockKind::Ways,
            BlockKind::Relations,
            BlockKind::Mixed,
        ] {
            let mut elements: Vec<u64> = infos
                .iter()
                .filter(|i| i.kind == kind)
                .map(|i| i.elements)
                .collect();
            if elements.is_empty() {
                continue;
            }
            elements.sort_unstable();
            let mut sizes: Vec<u64> = infos
                .iter()
                .filter(|i| i.kind == kind)
                .map(|i| i.compressed as u64)
                .collect();
            sizes.sort_unstable();

            writeln!(out, "  {}", kind.label())?;
            write_distribution_line(out, "    elements/block:", &elements, false)?;
            write_distribution_line(out, "    bytes/block:   ", &sizes, true)?;
        }
        Ok(())
    }

    /// Write the per-block table with optional head/tail limiting.
    ///
    /// `limit`: 0 = show all blocks, N = show first N and last N blocks.
    fn write_block_table_refs(
        out: &mut impl Write,
        infos: &[&BlockInfo],
        limit: usize,
    ) -> io::Result<()> {
        let has_raw = infos.iter().any(|i| i.raw.is_some());
        let truncate = limit > 0 && limit * 2 < infos.len();

        if has_raw {
            writeln!(
                out,
                "{:>6}  {:12}{:>8}  {:>10}  {:>10}",
                "Block", "Type", "Elements", "Compressed", "Raw"
            )?;
            if truncate {
                write_block_rows_raw(out, &infos[..limit])?;
                let omitted = infos.len() - limit * 2;
                writeln!(out, "   ...  ({omitted} blocks omitted)")?;
                write_block_rows_raw(out, &infos[infos.len() - limit..])?;
            } else {
                write_block_rows_raw(out, infos)?;
            }
        } else {
            writeln!(
                out,
                "{:>6}  {:12}{:>8}  {:>10}",
                "Block", "Type", "Elements", "Compressed"
            )?;
            if truncate {
                write_block_rows_compressed(out, &infos[..limit])?;
                let omitted = infos.len() - limit * 2;
                writeln!(out, "   ...  ({omitted} blocks omitted)")?;
                write_block_rows_compressed(out, &infos[infos.len() - limit..])?;
            } else {
                write_block_rows_compressed(out, infos)?;
            }
        }
        Ok(())
    }

    /// Write the block table with an anomaly reason column (no truncation).
    fn write_block_table_with_reason(
        out: &mut impl Write,
        infos: &[(&BlockInfo, &str)],
    ) -> io::Result<()> {
        let has_raw = infos.iter().any(|(i, _)| i.raw.is_some());

        if has_raw {
            writeln!(
                out,
                "{:>6}  {:12}{:>8}  {:>10}  {:>10}  Reason",
                "Block", "Type", "Elements", "Compressed", "Raw"
            )?;
            for (info, reason) in infos {
                writeln!(
                    out,
                    "{:>6}  {:12}{:>8}  {:>10}  {:>10}  {}",
                    info.number,
                    info.kind.label(),
                    info.elements,
                    format_size(info.compressed as u64),
                    format_size(info.raw.unwrap_or(0) as u64),
                    reason
                )?;
            }
        } else {
            writeln!(
                out,
                "{:>6}  {:12}{:>8}  {:>10}  Reason",
                "Block", "Type", "Elements", "Compressed"
            )?;
            for (info, reason) in infos {
                writeln!(
                    out,
                    "{:>6}  {:12}{:>8}  {:>10}  {}",
                    info.number,
                    info.kind.label(),
                    info.elements,
                    format_size(info.compressed as u64),
                    reason
                )?;
            }
        }
        Ok(())
    }

    fn write_id_ranges(
        out: &mut impl Write,
        node_ids: &TypeIdRange,
        way_ids: &TypeIdRange,
        rel_ids: &TypeIdRange,
    ) -> io::Result<()> {
        for (label, ids) in [
            ("Nodes:", node_ids),
            ("Ways:", way_ids),
            ("Relations:", rel_ids),
        ] {
            if ids.has_data() {
                writeln!(
                    out,
                    "  {:13}{} .. {}   (monotonic: {})",
                    label,
                    format_number_signed(ids.min_id),
                    format_number_signed(ids.max_id),
                    if ids.monotonic { "yes" } else { "no" }
                )?;
            }
        }
        Ok(())
    }

    #[allow(clippy::cast_precision_loss)]
    fn write_extended(out: &mut impl Write, ext: &ExtendedStats) -> io::Result<()> {
        writeln!(
            out,
            "Ordered:  {}",
            if ext.objects_ordered { "yes" } else { "no" }
        )?;
        if ext.has_timestamps() {
            writeln!(
                out,
                "Timestamps: {} .. {}",
                format_timestamp(ext.min_timestamp),
                format_timestamp(ext.max_timestamp)
            )?;
        }
        if ext.data_bbox.has_data() {
            let bb = &ext.data_bbox;
            writeln!(
                out,
                "Data bbox:  {},{},{},{}",
                bb.min_lon as f64 * 1e-9,
                bb.min_lat as f64 * 1e-9,
                bb.max_lon as f64 * 1e-9,
                bb.max_lat as f64 * 1e-9
            )?;
        }
        let m = &ext.metadata;
        if m.total > 0 {
            write_metadata_line(out, "All objects have:", m, true)?;
            write_metadata_line(out, "Some objects have:", m, false)?;
        }
        Ok(())
    }

    /// Retrieve a single value by dot-path key, for `--get` scripting.
    ///
    /// Returns `None` for unknown keys.
    pub fn get_value(&self, key: &str) -> Option<String> {
        self.get_value_inner(key)
    }

    #[allow(clippy::cast_precision_loss)]
    fn get_value_inner(&self, key: &str) -> Option<String> {
        match key {
            "file.name" => Some(self.file_name.clone()),
            "file.size" => Some(self.file_size.to_string()),
            "file.format" => Some("PBF".to_string()),
            "header.bbox" => self
                .header_meta
                .bbox
                .map(|(l, b, r, t)| format!("{l} {b} {r} {t}")),
            "header.writing_program" => self.header_meta.writing_program.clone(),
            "header.replication.url" => self.header_meta.replication_url.clone(),
            "header.replication.sequence" => {
                self.header_meta.replication_sequence.map(|s| s.to_string())
            }
            "header.replication.timestamp" => self
                .header_meta
                .replication_timestamp
                .map(|t| t.to_string()),
            "indexed" => Some(self.is_indexed.to_string()),
            "blocks.total" => Some(self.total_blocks.to_string()),
            "elements.nodes" => Some(self.state.node_count.to_string()),
            "elements.ways" => Some(self.state.way_count.to_string()),
            "elements.relations" => Some(self.state.relation_count.to_string()),
            "elements.total" => Some(
                (self.state.node_count + self.state.way_count + self.state.relation_count)
                    .to_string(),
            ),
            _ => self.get_extended_value(key),
        }
    }

    fn get_extended_value(&self, key: &str) -> Option<String> {
        let ext = self.state.extended.as_ref()?;
        match key {
            "data.objects_ordered" => Some(yes_no(ext.objects_ordered)),
            "data.timestamp.first" => {
                if ext.has_timestamps() {
                    Some(format_timestamp(ext.min_timestamp))
                } else {
                    Some(String::new())
                }
            }
            "data.timestamp.last" => {
                if ext.has_timestamps() {
                    Some(format_timestamp(ext.max_timestamp))
                } else {
                    Some(String::new())
                }
            }
            "data.bbox" => {
                if ext.data_bbox.has_data() {
                    let bb = &ext.data_bbox;
                    #[allow(clippy::cast_precision_loss)]
                    Some(format!(
                        "{} {} {} {}",
                        bb.min_lon as f64 * 1e-9,
                        bb.min_lat as f64 * 1e-9,
                        bb.max_lon as f64 * 1e-9,
                        bb.max_lat as f64 * 1e-9
                    ))
                } else {
                    Some(String::new())
                }
            }
            "data.count.nodes" => Some(self.state.node_count.to_string()),
            "data.count.ways" => Some(self.state.way_count.to_string()),
            "data.count.relations" => Some(self.state.relation_count.to_string()),
            "metadata.all_objects.version" => {
                Some(yes_no(ext.metadata.all_have(ext.metadata.has_version)))
            }
            "metadata.all_objects.timestamp" => {
                Some(yes_no(ext.metadata.all_have(ext.metadata.has_timestamp)))
            }
            "metadata.all_objects.changeset" => {
                Some(yes_no(ext.metadata.all_have(ext.metadata.has_changeset)))
            }
            "metadata.all_objects.uid" => Some(yes_no(ext.metadata.all_have(ext.metadata.has_uid))),
            "metadata.all_objects.user" => {
                Some(yes_no(ext.metadata.all_have(ext.metadata.has_user)))
            }
            "metadata.some_objects.version" => {
                Some(yes_no(ext.metadata.some_have(ext.metadata.has_version)))
            }
            "metadata.some_objects.timestamp" => {
                Some(yes_no(ext.metadata.some_have(ext.metadata.has_timestamp)))
            }
            "metadata.some_objects.changeset" => {
                Some(yes_no(ext.metadata.some_have(ext.metadata.has_changeset)))
            }
            "metadata.some_objects.uid" => {
                Some(yes_no(ext.metadata.some_have(ext.metadata.has_uid)))
            }
            "metadata.some_objects.user" => {
                Some(yes_no(ext.metadata.some_have(ext.metadata.has_user)))
            }
            _ => None,
        }
    }

    #[allow(clippy::cast_precision_loss)]
    fn write_locations(out: &mut impl Write, stats: &mut LocationStats) -> io::Result<()> {
        let total = stats.with_locations + stats.without_locations;
        if total == 0 {
            return writeln!(out, "Locations: no ways in file");
        }

        let with_pct = stats.with_locations as f64 / total as f64 * 100.0;
        let without_pct = stats.without_locations as f64 / total as f64 * 100.0;

        writeln!(
            out,
            "Ways with locations:    {} ({:.3}%)",
            format_number(stats.with_locations),
            with_pct
        )?;
        writeln!(
            out,
            "Ways without locations: {} ({:.3}%)",
            format_number(stats.without_locations),
            without_pct
        )?;

        if !stats.coord_counts.is_empty() {
            stats.coord_counts.sort_unstable();
            let len = stats.coord_counts.len();
            let min = stats.coord_counts[0];
            let max = stats.coord_counts[len - 1];
            let median = stats.coord_counts[len / 2];
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let p99_idx = ((len as f64 - 1.0) * 0.99) as usize;
            let p99 = stats.coord_counts[p99_idx.min(len - 1)];
            writeln!(
                out,
                "Coords per way:         min {min}, max {max}, median {median}, p99 {p99}"
            )?;
        }
        Ok(())
    }

    fn id_range_tuple(&self) -> Option<(&TypeIdRange, &TypeIdRange, &TypeIdRange)> {
        match (
            &self.state.node_ids,
            &self.state.way_ids,
            &self.state.relation_ids,
        ) {
            (Some(n), Some(w), Some(r)) => Some((n, w, r)),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Block table row helpers (free functions to avoid cognitive_complexity in methods)
// ---------------------------------------------------------------------------

fn write_block_rows_raw(out: &mut impl Write, infos: &[&BlockInfo]) -> io::Result<()> {
    for info in infos {
        writeln!(
            out,
            "{:>6}  {:12}{:>8}  {:>10}  {:>10}",
            info.number,
            info.kind.label(),
            info.elements,
            format_size(info.compressed as u64),
            format_size(info.raw.unwrap_or(0) as u64)
        )?;
    }
    Ok(())
}

fn write_block_rows_compressed(out: &mut impl Write, infos: &[&BlockInfo]) -> io::Result<()> {
    for info in infos {
        writeln!(
            out,
            "{:>6}  {:12}{:>8}  {:>10}",
            info.number,
            info.kind.label(),
            info.elements,
            format_size(info.compressed as u64)
        )?;
    }
    Ok(())
}

/// Write a distribution line (min/max/median/p99) for a sorted slice of values.
///
/// `label` is written as-is (should include leading whitespace and trailing colon).
/// `is_bytes` controls formatting: `true` uses `format_size`, `false` uses `format_number`.
#[allow(clippy::cast_precision_loss)]
fn write_distribution_line(
    out: &mut impl Write,
    label: &str,
    sorted: &[u64],
    is_bytes: bool,
) -> io::Result<()> {
    let fmt = if is_bytes { format_size } else { format_number };
    let len = sorted.len();
    let min = sorted[0];
    let max = sorted[len - 1];
    if len == 1 {
        return writeln!(out, "{label} {}", fmt(min));
    }
    let median = sorted[len / 2];
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let p99_idx = ((len as f64 - 1.0) * 0.99) as usize;
    let p99 = sorted[p99_idx.min(len - 1)];
    writeln!(
        out,
        "{label} min {}  max {}  median {}  p99 {}",
        fmt(min),
        fmt(max),
        fmt(median),
        fmt(p99),
    )
}

fn write_metadata_line(
    out: &mut impl Write,
    label: &str,
    m: &MetadataCoverage,
    all: bool,
) -> io::Result<()> {
    let check = |count: u64| -> bool {
        if all {
            m.all_have(count)
        } else {
            m.some_have(count)
        }
    };
    let mut attrs = Vec::new();
    if check(m.has_version) {
        attrs.push("version");
    }
    if check(m.has_timestamp) {
        attrs.push("timestamp");
    }
    if check(m.has_changeset) {
        attrs.push("changeset");
    }
    if check(m.has_uid) {
        attrs.push("uid");
    }
    if check(m.has_user) {
        attrs.push("user");
    }
    if attrs.is_empty() {
        writeln!(out, "  {label} (none)")
    } else {
        writeln!(out, "  {label} {}", attrs.join(", "))
    }
}
