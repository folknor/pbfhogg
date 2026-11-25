//! `geocode-query`: answer reverse-geocode queries over a built index, one
//! JSON object per query on stdout (the contract is docs/cli/commands.md,
//! "geocode-query").
//!
//! The points file is read whole and parsed before the index is queried, so
//! a malformed line refuses the run before any answer is printed.

use std::io::Write;
use std::path::Path;

use pbfhogg::geocode_index::reader::{Reader, ReverseResult};
use serde_json::{Value, json};

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

/// One query's answer as its output object.
fn answer(lat: f64, lon: f64, r: &ReverseResult<'_>) -> Value {
    json!({
        "lat": lat,
        "lon": lon,
        "address": r.address.as_ref().map(|a| json!({
            "lat_e7": a.lat_e7,
            "lon_e7": a.lon_e7,
            "house_number": a.house_number,
            "street": a.street,
            "place": a.place,
            "postcode": a.postcode,
            "distance_m": a.distance_m,
        })),
        "street": r.street.as_ref().map(|s| json!({
            "name": s.name,
            "snap_lat_e7": s.snap_lat_e7,
            "snap_lon_e7": s.snap_lon_e7,
            "distance_m": s.distance_m,
        })),
        "interpolation": r.interpolation.as_ref().map(|m| json!({
            "street": m.street,
            "place": m.place,
            "house_number": m.house_number,
            "distance_m": m.distance_m,
        })),
        "admin": r.admin.iter().map(|m| json!({
            "admin_level": m.admin_level,
            "name": m.name,
            "country_code": m.country_code,
            "area": m.area,
        })).collect::<Vec<_>>(),
    })
}

/// Every answer is computed, with a cancellation checkpoint per query,
/// before the completion decision; only then is anything printed, so an
/// interrupted run prints nothing.
pub(crate) fn run(
    index: &Path,
    points: &Path,
    host: &pbfhogg::host::Host,
) -> Result<(), Box<dyn std::error::Error>> {
    host.invocation.check()?;
    #[allow(clippy::disallowed_methods)]
    let text = std::fs::read_to_string(points)
        .map_err(|e| format!("failed to read {}: {e}", points.display()))?;
    let points = parse_points(&text)?;
    let reader = Reader::open(index)?;
    let mut report = Vec::new();
    for (i, &(lat, lon)) in points.iter().enumerate() {
        host.invocation.check()?;
        let result = reader
            .query(lat, lon)
            .map_err(|e| format!("points line {}: {e}", i + 1))?;
        serde_json::to_writer(&mut report, &answer(lat, lon, &result))?;
        report.push(b'\n');
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
    use super::parse_points;

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
