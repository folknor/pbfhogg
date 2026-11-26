//! `geocode-query`: answer reverse-geocode queries over a built index, one
//! JSON object per query on stdout (the contract is docs/cli/commands.md,
//! "geocode-query").
//!
//! The points file is read whole and parsed before the index is queried, so
//! a malformed line refuses the run before any answer is printed.

use std::error::Error;
use std::io::Write;
use std::path::Path;

use pbfhogg::GEOCODE_QUERY_BATCH_SIZE;
use pbfhogg::geocode_index::reader::{AdminMatch, Reader, ReverseResult};
use pbfhogg::host::{Executor, Host};
use rayon::prelude::{IndexedParallelIterator, IntoParallelRefIterator, ParallelIterator};
use serde::{Serialize, Serializer};

/// Every `(lat, lon)` of `text`: one query per line, latitude then longitude
/// in decimal degrees separated by ASCII whitespace. A blank line, a third
/// field or a value that is not a finite number is refused, naming its line.
fn parse_points(text: &str) -> Result<Vec<(f64, f64)>, String> {
    text.lines()
        .enumerate()
        .map(|(i, line)| {
            let at = |why: &str| format!("points line {}: {why}", i + 1);
            let mut fields = line.split_ascii_whitespace();
            let (Some(lat), Some(lon), None) = (fields.next(), fields.next(), fields.next()) else {
                return Err(at("expected `LAT LON`"));
            };
            let number = |field: &str| -> Result<f64, String> {
                field
                    .parse::<f64>()
                    .ok()
                    .filter(|v| v.is_finite())
                    .ok_or_else(|| at(&format!("{field:?} is not a finite number")))
            };
            let (lat, lon) = (number(lat)?, number(lon)?);
            if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
                return Err(at(
                    "latitude must be within -90 and 90 and longitude within -180 and 180",
                ));
            }
            Ok((lat, lon))
        })
        .collect()
}

// Keep the fields in the same sorted order as the original JSON object maps.
// The admin area is widened because the original Value stored it as an f64.
#[derive(Serialize)]
struct Answer<'a> {
    address: Option<Address<'a>>,
    #[serde(serialize_with = "serialize_admin")]
    admin: &'a [AdminMatch<'a>],
    interpolation: Option<Interpolation<'a>>,
    lat: f64,
    lon: f64,
    street: Option<Street<'a>>,
}

#[derive(Serialize)]
struct Address<'a> {
    distance_m: f64,
    house_number: &'a str,
    lat_e7: i32,
    lon_e7: i32,
    place: Option<&'a str>,
    postcode: Option<&'a str>,
    street: Option<&'a str>,
}

#[derive(Serialize)]
struct Admin<'a> {
    admin_level: u8,
    area: f64,
    country_code: Option<&'a str>,
    name: &'a str,
}

#[derive(Serialize)]
struct Interpolation<'a> {
    distance_m: f64,
    house_number: u32,
    place: Option<&'a str>,
    street: Option<&'a str>,
}

#[derive(Serialize)]
struct Street<'a> {
    distance_m: f64,
    name: &'a str,
    snap_lat_e7: i32,
    snap_lon_e7: i32,
}

fn serialize_admin<S: Serializer>(
    admin: &[AdminMatch<'_>],
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.collect_seq(admin.iter().map(|a| Admin {
        admin_level: a.admin_level,
        area: f64::from(a.area),
        country_code: a.country_code,
        name: a.name,
    }))
}

fn answer<'a>(lat: f64, lon: f64, result: &'a ReverseResult<'_>) -> Answer<'a> {
    Answer {
        address: result.address.as_ref().map(|a| Address {
            distance_m: a.distance_m,
            house_number: a.house_number,
            lat_e7: a.lat_e7,
            lon_e7: a.lon_e7,
            place: a.place,
            postcode: a.postcode,
            street: a.street,
        }),
        admin: &result.admin,
        interpolation: result.interpolation.as_ref().map(|m| Interpolation {
            distance_m: m.distance_m,
            house_number: m.house_number,
            place: m.place,
            street: m.street,
        }),
        lat,
        lon,
        street: result.street.as_ref().map(|s| Street {
            distance_m: s.distance_m,
            name: s.name,
            snap_lat_e7: s.snap_lat_e7,
            snap_lon_e7: s.snap_lon_e7,
        }),
    }
}

type QueryRow = Result<Vec<u8>, Box<dyn Error + Send + Sync>>;

fn append_batch(
    report: &mut Vec<u8>,
    points: &[(f64, f64)],
    offset: usize,
    reader: &Reader,
    host: &Host,
) -> Result<(), Box<dyn Error>> {
    let query = |(i, &(lat, lon)): (usize, &(f64, f64))| -> QueryRow {
        host.invocation.check()?;
        let result = reader
            .query(lat, lon)
            .map_err(|e| format!("points line {}: {e}", offset + i + 1))?;
        let mut row = serde_json::to_vec(&answer(lat, lon, &result))?;
        row.push(b'\n');
        Ok(row)
    };

    // Indexed collection preserves input order, including which query error
    // is returned when more than one row fails in this batch.
    let batch = || points.par_iter().enumerate().map(query).collect::<Vec<_>>();
    let rows = match &host.executor {
        _ if points.len() <= 1 => points.iter().enumerate().map(query).collect(),
        Executor::Ambient => batch(),
        #[allow(
            clippy::disallowed_methods,
            reason = "CLI dispatch to the host's rayon pool"
        )]
        Executor::Pool(pool) => pool.install(batch),
    };
    for row in rows {
        let row = row.map_err(|err| -> Box<dyn Error> { err })?;
        report.extend_from_slice(&row);
    }
    Ok(())
}

/// Keep serialized answers in memory until the final cancellation check.
/// Publishing batches as they finish would leave partial stdout on a query
/// error or cancellation. A scratch file would require a writable directory.
pub(crate) fn run(index: &Path, points: &Path, host: &Host) -> Result<(), Box<dyn Error>> {
    host.invocation.check()?;
    #[allow(clippy::disallowed_methods)]
    let text = std::fs::read_to_string(points)
        .map_err(|e| format!("failed to read {}: {e}", points.display()))?;
    let points = parse_points(&text)?;
    drop(text);
    let reader = Reader::open(index)?;
    let mut report = Vec::new();
    for (batch, points) in points.chunks(GEOCODE_QUERY_BATCH_SIZE).enumerate() {
        append_batch(
            &mut report,
            points,
            batch * GEOCODE_QUERY_BATCH_SIZE,
            &reader,
            host,
        )?;
    }

    host.invocation.check()?;
    let mut out = crate::output::buffered();
    out.write_all(&report)?;
    out.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use pbfhogg::geocode_index::reader::{
        AddressMatch, AdminMatch, InterpolationMatch, StreetMatch,
    };
    use serde_json::json;

    use super::{ReverseResult, answer, parse_points};

    #[test]
    fn answer_bytes_match_the_original_json_objects() {
        let area = 0.1_f32;
        let name = "A \"quoted\" name\nwith a line break";
        let result = ReverseResult {
            address: Some(AddressMatch {
                distance_m: 1.25,
                house_number: "7",
                lat_e7: 1,
                lon_e7: -2,
                place: None,
                postcode: Some("1234"),
                street: Some(name),
            }),
            admin: vec![AdminMatch {
                admin_level: 2,
                area,
                country_code: Some("DK"),
                name,
            }],
            interpolation: Some(InterpolationMatch {
                distance_m: 3.5,
                house_number: 9,
                place: Some(name),
                street: None,
            }),
            street: Some(StreetMatch {
                distance_m: f64::INFINITY,
                name,
                snap_lat_e7: -4,
                snap_lon_e7: 5,
            }),
        };
        let original = json!({
            "lat": -0.0,
            "lon": 1e-15,
            "address": {"lat_e7": 1, "lon_e7": -2, "house_number": "7", "street": name,
                        "place": null, "postcode": "1234", "distance_m": 1.25},
            "street": {"name": name, "snap_lat_e7": -4, "snap_lon_e7": 5, "distance_m": null},
            "interpolation": {"street": null, "place": name, "house_number": 9, "distance_m": 3.5},
            "admin": [{"admin_level": 2, "name": name, "country_code": "DK", "area": area}],
        });
        assert_eq!(
            serde_json::to_vec(&answer(-0.0, 1e-15, &result)).unwrap(),
            serde_json::to_vec(&original).unwrap()
        );
    }

    #[test]
    fn points_parse_strictly() {
        assert_eq!(
            parse_points("55.5 12.25\n-1e0\t180\n").unwrap(),
            vec![(55.5, 12.25), (-1.0, 180.0)]
        );
        let range = "latitude must be within -90 and 90 and longitude within -180 and 180";
        for (bad, needle) in [
            ("55.5\n", "points line 1: expected `LAT LON`"),
            ("1 2 3\n", "points line 1: expected `LAT LON`"),
            ("\n", "points line 1: expected `LAT LON`"),
            ("a 1\n", "points line 1: \"a\" is not a finite number"),
            ("NaN 1\n", "points line 1: \"NaN\" is not a finite number"),
            ("1 inf\n", "points line 1: \"inf\" is not a finite number"),
            ("91 0\n", range),
            ("0 -180.5\n", range),
        ] {
            pbfhogg_test_scratch::refused(parse_points(bad), needle);
        }
        assert!(parse_points("").unwrap().is_empty());
    }
}
