//! Download region datasets from Geofabrik.
//!
//! Replaces `download-regions.sh`. Downloads the latest PBF, optionally an
//! OSC diff, and generates an indexed PBF variant via `pbfhogg cat`.

use std::path::Path;

use crate::build;
use crate::error::DevError;
use crate::output;
use crate::tools;

// ---------------------------------------------------------------------------
// Region registry
// ---------------------------------------------------------------------------

struct RegionInfo {
    geofabrik_path: &'static str,
    approx_size: &'static str,
}

const REGIONS: &[(&str, RegionInfo)] = &[
    (
        "malta",
        RegionInfo {
            geofabrik_path: "europe/malta",
            approx_size: "8 MB",
        },
    ),
    (
        "greater-london",
        RegionInfo {
            geofabrik_path: "europe/united-kingdom/england/greater-london",
            approx_size: "116 MB",
        },
    ),
    (
        "switzerland",
        RegionInfo {
            geofabrik_path: "europe/switzerland",
            approx_size: "500 MB",
        },
    ),
    (
        "norway",
        RegionInfo {
            geofabrik_path: "europe/norway",
            approx_size: "1.3 GB",
        },
    ),
    (
        "japan",
        RegionInfo {
            geofabrik_path: "asia/japan",
            approx_size: "2.2 GB",
        },
    ),
    (
        "denmark",
        RegionInfo {
            geofabrik_path: "europe/denmark",
            approx_size: "465 MB",
        },
    ),
    (
        "germany",
        RegionInfo {
            geofabrik_path: "europe/germany",
            approx_size: "4.5 GB",
        },
    ),
    (
        "north-america",
        RegionInfo {
            geofabrik_path: "north-america",
            approx_size: "18.8 GB",
        },
    ),
];

fn lookup_region(name: &str) -> Result<&'static RegionInfo, DevError> {
    for &(region_name, ref info) in REGIONS {
        if region_name == name {
            return Ok(info);
        }
    }

    let valid: Vec<&str> = REGIONS.iter().map(|&(n, _)| n).collect();
    Err(DevError::Config(format!(
        "unknown region '{name}'. valid regions: {}",
        valid.join(", ")
    )))
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub fn run(
    region: &str,
    osc_url: Option<&str>,
    data_dir: &Path,
    workspace_root: &Path,
) -> Result<(), DevError> {
    let info = lookup_region(region)?;

    tools::check_curl()?;

    std::fs::create_dir_all(data_dir)?;

    output::download_msg(&format!("=== {region} ({}) ===", info.approx_size));

    // -- Download PBF --
    let pbf_url = format!(
        "https://download.geofabrik.de/{}-latest.osm.pbf",
        info.geofabrik_path
    );
    let pbf_dest = data_dir.join(format!("{region}-latest.osm.pbf"));

    if pbf_dest.exists() {
        output::download_msg(&format!("  SKIP (exists): {}", pbf_dest.display()));
    } else {
        output::download_msg(&format!("  GET: {pbf_url}"));
        tools::download_file(&pbf_url, &pbf_dest)?;
    }

    // -- Download OSC (if provided) --
    let osc_dest = if let Some(url) = osc_url {
        let filename = url
            .rsplit('/')
            .next()
            .ok_or_else(|| DevError::Config(format!("cannot extract filename from URL: {url}")))?;
        let dest = data_dir.join(filename);

        if dest.exists() {
            output::download_msg(&format!("  SKIP (exists): {}", dest.display()));
        } else {
            output::download_msg(&format!("  GET: {url}"));
            tools::download_file(url, &dest)?;
        }

        Some(dest)
    } else {
        None
    };

    // -- Generate indexed PBF --
    let indexed_dest = data_dir.join(format!("{region}-latest-with-indexdata.osm.pbf"));

    if indexed_dest.exists() {
        output::download_msg(&format!(
            "  SKIP (exists): {}",
            indexed_dest.display()
        ));
    } else {
        output::download_msg("  generating indexed PBF via cat");

        let binary = build::cargo_build(&build::BuildConfig::release_cli(), workspace_root)?;
        let binary_str = binary.display().to_string();
        let pbf_str = pbf_dest.display().to_string();
        let indexed_str = indexed_dest.display().to_string();

        let captured = output::run_captured(
            &binary_str,
            &["cat", &pbf_str, "--type", "node,way,relation", "-o", &indexed_str],
            workspace_root,
        )?;

        if !captured.status.success() {
            let stderr = String::from_utf8_lossy(&captured.stderr);
            return Err(DevError::Subprocess {
                program: binary_str,
                code: captured.status.code(),
                stderr: stderr.into_owned(),
            });
        }
    }

    // -- Summary --
    output::download_msg("=== Summary ===");
    output::download_msg(&format!("  PBF: {}", pbf_dest.display()));
    if let Some(ref osc) = osc_dest {
        output::download_msg(&format!("  OSC: {}", osc.display()));
    }
    output::download_msg(&format!("  Indexed: {}", indexed_dest.display()));

    Ok(())
}
