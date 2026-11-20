//! Reader contracts from index records encoded without the command builder.

use pbfhogg::geocode_index::format::{self, AddrPoint, GeoCell, Header};
use std::path::Path;

fn write_index(dir: &Path) {
    std::fs::create_dir(dir).expect("create index");
    let header = Header {
        street_cell_level: 16,
        coarse_cell_level: 14,
        admin_cell_level: 10,
        admin_tolerance_m: 0.0,
        fine_search_radius_m: 10.0,
        coarse_search_radius_m: 20.0,
        replication_sequence: None,
        replication_timestamp: None,
        addr_point_count: 1,
        street_way_count: 1,
        interp_way_count: 1,
        admin_polygon_count: 1,
        geo_cell_count: 2,
        coarse_cell_count: 0,
        admin_cell_count: 1,
        input_xxh3_128: 0,
        input_bytes: 0,
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
    write_offset_records(dir);
    write_lists_and_address(dir);
}

/// Populate every offset-bearing record without depending on the builder.
/// Geometry is intentionally unreferenced by the query entry lists.
fn write_offset_records(dir: &Path) {
    let street = format::StreetWay {
        node_offset: 0,
        name_offset: 4,
        node_count: 1,
    };
    let interpolation = format::InterpWay {
        node_offset: 0,
        parent_offset: 4,
        start_number: 10,
        end_number: 20,
        node_count: 1,
        interpolation_type: 0,
        parent_kind: 0,
    };
    let polygon = format::AdminPolygon {
        area: 1.0,
        vertex_offset: 0,
        vertex_count: 1,
        name_offset: 4,
        country_code_offset: 0,
        admin_level: 6,
    };
    let admin = format::AdminCell {
        cell_id: 1,
        entries_offset: 0,
    };
    for (file, bytes) in [
        (format::FILE_STREET_WAYS, street.to_bytes().to_vec()),
        (format::FILE_INTERP_WAYS, interpolation.to_bytes().to_vec()),
        (format::FILE_ADMIN_POLYGONS, polygon.to_bytes().to_vec()),
        (format::FILE_ADMIN_CELLS, admin.to_bytes().to_vec()),
    ] {
        std::fs::write(dir.join(file), bytes).expect("offset-bearing record");
    }
    for file in [
        format::FILE_STREET_NODES,
        format::FILE_INTERP_NODES,
        format::FILE_ADMIN_VERTICES,
    ] {
        std::fs::write(dir.join(file), [0u8; 8]).expect("one coordinate");
    }
    std::fs::write(dir.join(format::FILE_ADMIN_ENTRIES), 0u16.to_le_bytes())
        .expect("empty admin list");
}

fn write_lists_and_address(dir: &Path) {
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

/// Reader open-time validation must cover every offset-into-a-file
/// relationship in the format, not just the geo entry lists. Validating some
/// of the kinds and not the others is how a truncated file opens successfully
/// and then silently answers with incomplete results: every read path is
/// individually bounds-safe and returns "nothing here" rather than an error,
/// so the caller cannot tell a truncated list from an empty one.
#[test]
fn reader_rejects_every_out_of_bounds_offset_kind() {
    let dir = crate::common::test_scratch();
    let good = dir.path().join("good");
    write_index(&good);
    pbfhogg::geocode_index::reader::Reader::open(&good)
        .expect("the pristine index must open, or every case below is vacuous");

    // (file to corrupt, byte range holding the offset, substring of the
    // expected error). One case per offset kind in the format.
    let cases: &[(&str, std::ops::Range<usize>, &str)] = &[
        ("admin_cells.bin", 8..16, "admin entry"),
        ("street_ways.bin", 0..8, "street way node"),
        ("interp_ways.bin", 0..8, "interpolation way node"),
        ("admin_polygons.bin", 4..12, "admin polygon vertex"),
    ];
    for (file, range, expected) in cases {
        let index_dir = dir.path().join(format!("case-{file}"));
        write_index(&index_dir);
        let path = index_dir.join(file);
        let mut bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("read {file}: {e}"));
        assert!(
            bytes.len() >= range.end,
            "{file} is empty in the fixture, so this case proves nothing"
        );
        let poison = vec![0xFEu8; range.len()];
        bytes[range.clone()].copy_from_slice(&poison);
        std::fs::write(&path, bytes).unwrap_or_else(|e| panic!("write {file}: {e}"));

        let err = pbfhogg::geocode_index::reader::Reader::open(&index_dir)
            .err()
            .unwrap_or_else(|| panic!("{file}: out-of-bounds offset must fail open, not open"));
        assert!(
            err.to_string().contains(expected),
            "{file}: expected an error mentioning {expected}, got: {err}"
        );
    }
}

/// An entry naming a record past its file keeps every validated length and
/// span intact, so it opens; the lookup that reads it refuses instead of
/// answering with no address as if none were there.
#[test]
fn a_dangling_address_entry_fails_the_query() {
    use pbfhogg::geocode_index::reader::{IndexReference, QueryError, Reader};

    let dir = crate::common::test_scratch();
    let index_dir = dir.path().join("index");
    write_index(&index_dir);
    std::fs::write(
        index_dir.join(format::FILE_ADDR_ENTRIES),
        [1, 0, 1, 0, 0, 0],
    )
    .expect("name address point 1 of 1");
    let reader = Reader::open(&index_dir).expect("entry contents are not checked at open");
    let expected = QueryError::DanglingReference {
        reference: IndexReference::AddressPoint,
        index: 1,
        count: 1,
    };
    assert_eq!(reader.query(55.7, 12.5).err(), Some(expected));
    assert_eq!(reader.candidates(55.7, 12.5).err(), Some(expected));
}

#[test]
fn a_dangling_coarse_address_entry_fails_the_query() {
    use pbfhogg::geocode_index::reader::{IndexReference, QueryError, Reader};

    let dir = crate::common::test_scratch();
    let index_dir = dir.path().join("index");
    write_index(&index_dir);

    // The address at 55.7 is about 15 m away, between the fixture's fine
    // and coarse radii. The fine list stays valid throughout the test.
    let (lat, lon) = (55.700_135, 12.5);
    let header_path = index_dir.join(format::FILE_HEADER);
    let bytes: [u8; format::HEADER_SIZE] = std::fs::read(&header_path)
        .expect("read header")
        .try_into()
        .expect("header size");
    let mut header = Header::from_bytes(&bytes).expect("decode header");
    header.coarse_cell_count = 1;
    std::fs::write(header_path, header.to_bytes()).expect("write header");
    let cell_id = s2::cellid::CellID::from(s2::latlng::LatLng::from_degrees(lat, lon))
        .parent(u64::from(header.coarse_cell_level))
        .0;
    std::fs::write(
        index_dir.join(format::FILE_COARSE_GEO_CELLS),
        GeoCell {
            cell_id,
            street_offset: 0,
            addr_offset: 0,
            interp_offset: 0,
        }
        .to_bytes(),
    )
    .expect("coarse cell");

    let entries_path = index_dir.join(format::FILE_COARSE_ADDR_ENTRIES);
    std::fs::write(&entries_path, [1, 0, 0, 0, 0, 0]).expect("valid coarse address index");
    {
        let reader = Reader::open(&index_dir).expect("open valid index");
        let result = reader.query(lat, lon).expect("query valid index");
        let address = result.address.expect("coarse address hit");
        assert!(address.distance_m > f64::from(header.fine_search_radius_m));
        assert!(address.distance_m < f64::from(header.coarse_search_radius_m));
        let candidates = reader.candidates(lat, lon).expect("read valid candidates");
        assert_eq!(candidates.addresses.len(), 1);
    }

    std::fs::write(entries_path, [1, 0, 1, 0, 0, 0]).expect("name address point 1 of 1");
    let reader = Reader::open(&index_dir).expect("entry contents are not checked at open");
    let expected = QueryError::DanglingReference {
        reference: IndexReference::AddressPoint,
        index: 1,
        count: 1,
    };
    assert_eq!(reader.query(lat, lon).err(), Some(expected));
    assert_eq!(reader.candidates(lat, lon).err(), Some(expected));
}

/// The admin searches binary-search their cells, so open refuses admin
/// cells out of order or repeated, as it does geo cells.
#[test]
fn reader_rejects_admin_cells_out_of_order() {
    for ids in [[2u64, 1], [1, 1]] {
        let dir = crate::common::test_scratch();
        let index_dir = dir.path().join("index");
        write_index(&index_dir);
        let header_path = index_dir.join(format::FILE_HEADER);
        let bytes: [u8; format::HEADER_SIZE] = std::fs::read(&header_path)
            .expect("read header")
            .try_into()
            .expect("header size");
        let mut header = Header::from_bytes(&bytes).expect("decode header");
        header.admin_cell_count = 2;
        std::fs::write(&header_path, header.to_bytes()).expect("write header");
        let mut cells = Vec::new();
        for cell_id in ids {
            cells.extend_from_slice(
                &format::AdminCell {
                    cell_id,
                    entries_offset: 0,
                }
                .to_bytes(),
            );
        }
        std::fs::write(index_dir.join(format::FILE_ADMIN_CELLS), cells).expect("admin cells");
        let err = pbfhogg::geocode_index::reader::Reader::open(&index_dir)
            .err()
            .expect("unordered admin cells must fail open");
        assert!(
            err.report().contains("cell IDs are not strictly ascending"),
            "{ids:?}: unexpected error: {err}"
        );
    }
}

/// Offset 0 must hold the empty string, not merely decode: a pool whose
/// first entry is a one-byte string would turn every absent name into it.
#[test]
fn reader_rejects_a_nonempty_string_sentinel() {
    let dir = crate::common::test_scratch();
    let index_dir = dir.path().join("index");
    write_index(&index_dir);
    let path = index_dir.join(format::FILE_STRINGS);
    let mut strings = std::fs::read(&path).expect("read strings");
    // Offset 0 now frames the one-byte entry holding the next length byte;
    // every other entry still decodes where it did.
    strings[0] = 1;
    std::fs::write(&path, strings).expect("write strings");
    let err = pbfhogg::geocode_index::reader::Reader::open(&index_dir)
        .err()
        .expect("a nonempty sentinel must fail open");
    assert!(
        err.report().contains("empty-string sentinel"),
        "unexpected error: {err}"
    );
}

/// Ranked lookup compares areas and slots polygons by level, so an area or
/// level the builder cannot write is refused at open rather than silently
/// excluding or misranking the polygon.
#[test]
fn reader_rejects_admin_polygon_area_and_level_outside_the_format() {
    let base = format::AdminPolygon {
        area: 1.0,
        vertex_offset: 0,
        vertex_count: 1,
        name_offset: 4,
        country_code_offset: 0,
        admin_level: 6,
    };
    let mut cases = Vec::new();
    for area in [f32::NAN, -f32::NAN, f32::INFINITY, -1.0, -0.0] {
        cases.push((format::AdminPolygon { area, ..base }, "area"));
    }
    for admin_level in [0, 1, 12, 255] {
        cases.push((
            format::AdminPolygon {
                admin_level,
                ..base
            },
            "level",
        ));
    }
    for (polygon, expected) in cases {
        let dir = crate::common::test_scratch();
        let index_dir = dir.path().join("index");
        write_index(&index_dir);
        std::fs::write(
            index_dir.join(format::FILE_ADMIN_POLYGONS),
            polygon.to_bytes(),
        )
        .expect("write polygon");
        let err = pbfhogg::geocode_index::reader::Reader::open(&index_dir)
            .err()
            .unwrap_or_else(|| panic!("{polygon:?} must fail open"));
        assert!(
            err.report().contains(expected),
            "{polygon:?}: unexpected error: {err}"
        );
    }
    for (area, admin_level) in [(0.0, 2), (1.0, 10), (1.0, format::POSTAL_BOUNDARY_LEVEL)] {
        let dir = crate::common::test_scratch();
        let index_dir = dir.path().join("index");
        write_index(&index_dir);
        std::fs::write(
            index_dir.join(format::FILE_ADMIN_POLYGONS),
            format::AdminPolygon {
                area,
                admin_level,
                ..base
            }
            .to_bytes(),
        )
        .expect("write polygon");
        pbfhogg::geocode_index::reader::Reader::open(&index_dir)
            .unwrap_or_else(|e| panic!("area {area} level {admin_level} must open: {e}"));
    }
}
