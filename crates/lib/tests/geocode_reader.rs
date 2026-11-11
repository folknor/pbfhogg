//! Reader contracts from index records encoded without the command builder.

use pbfhogg::geocode_index::format::{self, AddrPoint, GeoCell, Header};
use std::path::Path;

fn write_index(dir: &Path) {
    std::fs::create_dir(dir).expect("create index");
    let header = Header {
        format_version: format::FORMAT_VERSION,
        street_cell_level: 16,
        coarse_cell_level: 14,
        admin_cell_level: 10,
        admin_tolerance_m: 0.0,
        fine_search_radius_m: 10.0,
        coarse_search_radius_m: 20.0,
        replication_sequence: 0,
        replication_timestamp: 0,
        addr_point_count: 1,
        street_way_count: 0,
        interp_way_count: 0,
        admin_polygon_count: 0,
        geo_cell_count: 2,
        coarse_cell_count: 0,
        admin_cell_count: 0,
    };
    std::fs::write(dir.join(format::FILE_HEADER), header.to_bytes()).expect("header");
    for file in [
        format::FILE_STREET_WAYS,
        format::FILE_STREET_NODES,
        format::FILE_INTERP_WAYS,
        format::FILE_INTERP_NODES,
        format::FILE_ADMIN_CELLS,
        format::FILE_ADMIN_ENTRIES,
        format::FILE_ADMIN_POLYGONS,
        format::FILE_ADMIN_VERTICES,
        format::FILE_COARSE_GEO_CELLS,
    ] {
        std::fs::write(dir.join(file), []).expect("empty record file");
    }
    // Every empty list has a u16 count, at offset zero.
    for file in [
        format::FILE_STREET_ENTRIES,
        format::FILE_INTERP_ENTRIES,
        format::FILE_COARSE_STREET_ENTRIES,
        format::FILE_COARSE_ADDR_ENTRIES,
        format::FILE_COARSE_INTERP_ENTRIES,
    ] {
        std::fs::write(dir.join(file), 0u16.to_le_bytes()).expect("empty list");
    }
    let cell = s2::cellid::CellID::from(s2::latlng::LatLng::from_degrees(55.7, 12.5))
        .parent(16)
        .0;
    let mut cells = Vec::new();
    for cell_id in [cell, cell + (2 * cell.isolate_lowest_one())] {
        cells.extend_from_slice(
            &GeoCell {
                cell_id,
                street_offset: 0,
                addr_offset: 0,
                interp_offset: 0,
            }
            .to_bytes(),
        );
    }
    std::fs::write(dir.join(format::FILE_GEO_CELLS), cells).expect("cells");
    std::fs::write(dir.join(format::FILE_ADDR_ENTRIES), [1, 0, 0, 0, 0, 0])
        .expect("one address index");
    std::fs::write(
        dir.join(format::FILE_ADDR_POINTS),
        AddrPoint {
            lat_e7: 557_000_000,
            lon_e7: 125_000_000,
            housenumber_offset: 1,
            street_offset: 4,
            postcode_offset: 0,
            place_offset: 0,
        }
        .to_bytes(),
    )
    .expect("address");
    let mut strings = vec![0];
    for value in ["10", "Main Street"] {
        strings.push(u8::try_from(value.len()).expect("short fixture string"));
        strings.extend_from_slice(value.as_bytes());
    }
    std::fs::write(dir.join(format::FILE_STRINGS), strings).expect("strings");
}

#[test]
fn independently_encoded_address_is_queryable() {
    let scratch = crate::common::test_scratch();
    let dir = scratch.path().join("index");
    write_index(&dir);
    let reader = pbfhogg::geocode_index::reader::Reader::open(&dir).expect("valid index");
    let result = reader.query(55.7, 12.5).expect("valid query");
    let address = result.address.expect("encoded address must answer");
    assert_eq!(address.house_number, "10");
    assert_eq!(address.street, Some("Main Street"));
    assert!(reader.query(f64::NAN, 12.5).is_err());
    assert!(reader.candidates(55.7, 181.0).is_err());
}

#[test]
fn reader_rejects_hostile_entry_offset_instead_of_panicking() {
    let dir = crate::common::test_scratch();
    let index_dir = dir.path().join("index");
    write_index(&index_dir);

    let cells_path = index_dir.join("geo_cells.bin");
    let mut cells = std::fs::read(&cells_path).expect("read cells");
    cells[16..24].copy_from_slice(&(u64::MAX - 1).to_le_bytes());
    std::fs::write(&cells_path, cells).expect("corrupt offset");
    let err = pbfhogg::geocode_index::reader::Reader::open(&index_dir)
        .err()
        .expect("hostile offset must be a typed open error");
    assert!(
        err.to_string().contains("address entry offset"),
        "unexpected error: {err}"
    );
}

#[test]
fn reader_rejects_cells_out_of_order() {
    let dir = crate::common::test_scratch();
    let index_dir = dir.path().join("index");
    write_index(&index_dir);

    let cells_path = index_dir.join("geo_cells.bin");
    let mut cells = std::fs::read(&cells_path).expect("read cells");
    assert!(
        cells.len() >= 2 * GeoCell::SIZE,
        "the fixture needs two fine cells"
    );
    let (first, rest) = cells.split_at_mut(GeoCell::SIZE);
    first.swap_with_slice(&mut rest[..GeoCell::SIZE]);
    std::fs::write(&cells_path, cells).expect("reorder cells");
    let err = pbfhogg::geocode_index::reader::Reader::open(&index_dir)
        .err()
        .expect("out-of-order cells must be a typed open error");
    assert!(
        err.to_string()
            .contains("cell IDs are not strictly ascending"),
        "unexpected error: {err}"
    );
}

#[test]
fn reader_open_failure_names_the_missing_file() {
    let dir = crate::common::test_scratch();
    let missing = dir.path().join("no-such-index");
    let err = pbfhogg::geocode_index::reader::Reader::open(&missing)
        .err()
        .expect("a missing index must fail open");
    let report = err.report();
    assert!(
        report.contains(&missing.display().to_string()),
        "the error must name the missing index's header: {report}"
    );

    let index_dir = dir.path().join("index");
    write_index(&index_dir);

    let strings = index_dir.join("strings.bin");
    std::fs::remove_file(&strings).expect("remove a data file");
    let err = pbfhogg::geocode_index::reader::Reader::open(&index_dir)
        .err()
        .expect("an index missing a data file must fail open");
    let report = err.report();
    assert!(
        report.contains(&strings.display().to_string()),
        "the error must name the missing data file: {report}"
    );
}
