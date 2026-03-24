//! Integration tests for the reverse geocoding index builder and reader.
//!
//! Requires Denmark PBF with indexdata. Skipped if the file doesn't exist.
//! Run with: `cargo test --test geocode_index -- --ignored`

use std::path::Path;

fn denmark_indexed_path() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("PBFHOGG_TEST_PBF_INDEXED") {
        return std::path::PathBuf::from(p);
    }
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("data/denmark-20260220-seq4704-with-indexdata.osm.pbf")
}

fn output_dir() -> std::path::PathBuf {
    // Use existing scratch directory if available (built by brokkr run),
    // otherwise use target/geocode-test.
    let scratch = Path::new(env!("CARGO_MANIFEST_DIR")).join("data/scratch/geocode-denmark");
    if scratch.join("geocode_header.bin").exists() {
        return scratch;
    }
    Path::new(env!("CARGO_MANIFEST_DIR")).join("target/geocode-test")
}

fn ensure_index_exists() -> Option<std::path::PathBuf> {
    let dir = output_dir();
    if dir.join("geocode_header.bin").exists() {
        return Some(dir);
    }
    // Try building (only reasonable in release mode)
    let input = denmark_indexed_path();
    if !input.exists() {
        return None;
    }
    let build_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/geocode-test");
    if build_dir.exists() {
        std::fs::remove_dir_all(&build_dir).ok();
    }
    let config = pbfhogg::geocode_index::builder::BuildConfig {
        input_path: input,
        output_dir: build_dir.clone(),
        force: false,
        ..Default::default()
    };
    pbfhogg::geocode_index::builder::build_geocode_index(&config).ok()?;
    Some(build_dir)
}

/// Build the index and verify counts are reasonable for Denmark.
#[test]
#[ignore]
fn build_denmark_index() {
    let input = denmark_indexed_path();
    if !input.exists() {
        eprintln!("Skipping: {} not found", input.display());
        return;
    }

    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/geocode-test-build");
    if dir.exists() {
        std::fs::remove_dir_all(&dir).ok();
    }

    let config = pbfhogg::geocode_index::builder::BuildConfig {
        input_path: input,
        output_dir: dir.clone(),
        force: false,
        ..Default::default()
    };
    let stats = pbfhogg::geocode_index::builder::build_geocode_index(&config)
        .expect("build should succeed");

    assert!(stats.addr_points > 2_000_000, "expected >2M addr, got {}", stats.addr_points);
    assert!(stats.street_ways > 200_000, "expected >200K streets, got {}", stats.street_ways);
    assert!(stats.admin_polygons > 100, "expected >100 admin, got {}", stats.admin_polygons);
    assert!(stats.fine_cells > 100_000, "expected >100K fine cells, got {}", stats.fine_cells);

    // Verify all index files exist
    let expected_files = [
        "geocode_header.bin", "geo_cells.bin", "street_entries.bin",
        "addr_entries.bin", "interp_entries.bin", "coarse_geo_cells.bin",
        "coarse_street_entries.bin", "coarse_addr_entries.bin",
        "coarse_interp_entries.bin", "street_ways.bin", "street_nodes.bin",
        "addr_points.bin", "interp_ways.bin", "interp_nodes.bin",
        "admin_cells.bin", "admin_entries.bin", "admin_polygons.bin",
        "admin_vertices.bin", "strings.bin",
    ];
    for name in &expected_files {
        assert!(dir.join(name).exists(), "missing index file: {name}");
    }
}

/// Query Copenhagen City Hall and verify address/street/admin results.
#[test]
#[ignore]
fn query_copenhagen() {
    let Some(dir) = ensure_index_exists() else {
        eprintln!("Skipping: index not available");
        return;
    };
    let reader = pbfhogg::geocode_index::reader::Reader::open(&dir)
        .expect("reader should open");

    // 55.6761°N, 12.5683°E — Copenhagen City Hall / Rådhuspladsen
    let result = reader.query(55.6761, 12.5683);

    assert!(
        result.address.is_some() || result.street.is_some(),
        "expected address or street near Copenhagen City Hall"
    );
    assert!(!result.admin.is_empty(), "expected admin matches");

    // Country level
    let country = result.admin.iter().find(|a| a.admin_level == 2);
    assert!(country.is_some(), "expected country-level admin");
    if let Some(c) = country {
        assert!(
            c.name.contains("Danmark") || c.name.contains("Denmark"),
            "expected Danmark/Denmark, got '{}'", c.name
        );
    }

    if let Some(addr) = &result.address {
        eprintln!("Address: {} {} (dist: {:.1}m)", addr.street, addr.house_number, addr.distance_m);
    }
    if let Some(st) = &result.street {
        eprintln!("Street: {} (dist: {:.1}m)", st.name, st.distance_m);
    }
}

/// Rural query: admin should resolve even where streets are sparse.
#[test]
#[ignore]
fn query_rural_jutland() {
    let Some(dir) = ensure_index_exists() else {
        eprintln!("Skipping: index not available");
        return;
    };
    let reader = pbfhogg::geocode_index::reader::Reader::open(&dir)
        .expect("reader should open");

    // 57.5°N, 10.0°E — sparse area in northern Jutland
    let result = reader.query(57.5, 10.0);
    let country = result.admin.iter().find(|a| a.admin_level == 2);
    assert!(country.is_some(), "rural point should resolve country");
}

/// Outside Denmark: no results expected.
#[test]
#[ignore]
fn query_north_sea() {
    let Some(dir) = ensure_index_exists() else {
        eprintln!("Skipping: index not available");
        return;
    };
    let reader = pbfhogg::geocode_index::reader::Reader::open(&dir)
        .expect("reader should open");

    // 56.0°N, 4.0°E — North Sea
    let result = reader.query(56.0, 4.0);
    assert!(result.address.is_none(), "no address in the North Sea");
    assert!(result.street.is_none(), "no street in the North Sea");
}

/// API equivalence: query() should produce the same result as candidates().into_result().
#[test]
#[ignore]
fn api_equivalence() {
    let Some(dir) = ensure_index_exists() else {
        eprintln!("Skipping: index not available");
        return;
    };
    let reader = pbfhogg::geocode_index::reader::Reader::open(&dir)
        .expect("reader should open");

    let points = [(55.6761, 12.5683), (56.15, 10.2), (55.4, 12.3)];

    for &(lat, lon) in &points {
        let rq = reader.query(lat, lon);
        let rc = reader.candidates(lat, lon).into_result(&reader);

        assert_eq!(rq.address.is_some(), rc.address.is_some(),
            "disagree on address at ({lat}, {lon})");
        assert_eq!(rq.street.is_some(), rc.street.is_some(),
            "disagree on street at ({lat}, {lon})");

        let q_levels: Vec<u8> = rq.admin.iter().map(|a| a.admin_level).collect();
        let c_levels: Vec<u8> = rc.admin.iter().map(|a| a.admin_level).collect();
        assert_eq!(q_levels, c_levels,
            "disagree on admin levels at ({lat}, {lon})");

        if let (Some(qa), Some(ca)) = (&rq.address, &rc.address) {
            assert_eq!(qa.street, ca.street,
                "disagree on address street at ({lat}, {lon})");
        }
    }
}

/// Coarse fallback should find results in areas with sparse street coverage.
#[test]
#[ignore]
fn coarse_fallback() {
    let Some(dir) = ensure_index_exists() else {
        eprintln!("Skipping: index not available");
        return;
    };
    let reader = pbfhogg::geocode_index::reader::Reader::open(&dir)
        .expect("reader should open");

    // Thy National Park, northern Jutland: 56.92°N, 8.52°E
    let result = reader.query(56.92, 8.52);

    // Admin should resolve regardless
    let country = result.admin.iter().find(|a| a.admin_level == 2);
    assert!(country.is_some(), "should find country in rural Thy");

    eprintln!("Thy: address={} street={} admin={}",
        result.address.is_some(), result.street.is_some(), result.admin.len());
}
