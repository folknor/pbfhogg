//! `merge`: the merge-workload bench's instrument. It runs the
//! apply-changes merge of a base PBF and an OSC into an output and prints
//! its timing and statistics. The timer stops where publication begins;
//! the measured process wall still includes publication and shutdown. The
//! merge runs as `apply-changes --force` would, so an unindexed base is
//! accepted, with automatic parallelism, no locations on ways and the
//! header's replication fields left to the command's policy.

use pbfhogg::host::Host;
use pbfhogg::write::{OutputBackend, WriteOptions};
use pbfhogg::writer::Compression;
use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::Instant;

/// The output backend a merge writes through.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum IoMode {
    Buffered,
    Uring,
}

/// Where a merge writes, and how.
pub(crate) struct Target<'a> {
    pub(crate) output: &'a Path,
    pub(crate) overwrite: bool,
    pub(crate) staging_dir: Option<&'a Path>,
    pub(crate) compression: Compression,
    pub(crate) io_mode: IoMode,
}

pub(crate) fn merge(
    base: &Path,
    changes: &Path,
    target: &Target<'_>,
    host: &Host,
) -> Result<(), crate::Failure> {
    let backend = OutputBackend::from_flags(false, target.io_mode == IoMode::Uring);
    let output = pbfhogg::Output::with_staging(
        target.output,
        pbfhogg::Overwrite::from_flag(target.overwrite),
        pbfhogg::Staging::dir(target.staging_dir),
        &host.diagnostics,
    )?;

    // The library publishes the output (fsync, rename, directory sync)
    // inside `merge`, which is not the measured work, so the output stops
    // the clock itself when publication begins.
    let start = Instant::now();
    let work_elapsed = Arc::new(OnceLock::new());
    let output = {
        let work_elapsed = Arc::clone(&work_elapsed);
        output.before_publish(move || {
            work_elapsed.set(start.elapsed()).ok();
        })
    };
    let opts = pbfhogg::apply_changes::MergeOptions {
        write: WriteOptions {
            compression: target.compression,
            backend,
            direct_reads: false,
            force: true,
        },
        locations_on_ways: None,
        parallelism: pbfhogg::Parallelism::Auto,
        allow_version_regression: false,
    };
    let stats = pbfhogg::apply_changes::merge(
        base,
        changes,
        output,
        &opts,
        &pbfhogg::HeaderOverrides::default(),
        host,
    )?;
    let Some(elapsed) = work_elapsed.get() else {
        return Err(crate::Failure::Message(
            "merge: the output was published without stopping the timer".to_owned(),
        ));
    };

    #[allow(clippy::cast_precision_loss, reason = "a size in megabytes")]
    let output_mb = std::fs::metadata(target.output)
        .map(|m| m.len() as f64 / 1_000_000.0)
        .unwrap_or(0.0);

    eprintln!("elapsed_ms={}", elapsed.as_millis());
    eprintln!("base_nodes={}", stats.base_nodes);
    eprintln!("base_ways={}", stats.base_ways);
    eprintln!("base_relations={}", stats.base_relations);
    eprintln!("diff_nodes={}", stats.diff_nodes);
    eprintln!("diff_ways={}", stats.diff_ways);
    eprintln!("diff_relations={}", stats.diff_relations);
    eprintln!("blobs_passthrough={}", stats.blobs_passthrough);
    eprintln!("blobs_rewritten={}", stats.blobs_rewritten);
    eprintln!("bytes_passthrough={}", stats.bytes_passthrough);
    eprintln!("bytes_rewritten={}", stats.bytes_rewritten);
    eprintln!("diff_heap_bytes={}", stats.diff_heap_bytes);
    eprintln!("output_mb={output_mb:.2}");
    // Peak RSS is a best-effort diagnostic; omit the field when the probe
    // cannot provide a reading instead of printing a fabricated zero.
    if let Some(peak_kb) = pbfhogg::process::read_peak_rss_kb() {
        eprintln!("peak_rss_kb={peak_kb}");
    }
    Ok(())
}
