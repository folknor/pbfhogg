//! Verify: whether osmium reads a polygon file as the region pbfhogg reads.
//!
//! `verify extract` compares pbfhogg's polygon extract with osmium's on the
//! same GeoJSON file, which is a comparison only where both tools read one
//! region from it. pbfhogg reads more GeoJSON than osmium does, and reads part
//! of it differently (pbfhogg's DEVIATIONS.md, "extract: polygon input beyond
//! osmium"): it unions every feature of a FeatureCollection where osmium takes
//! the first, closes an open ring where osmium uses it as given, and unwraps a
//! ring with an edge spanning more than 180 degrees of longitude where osmium
//! takes that edge across the globe. On such a file the tools select different
//! regions by design, so the check refuses it before any run rather than
//! failing pbfhogg. What either tool refuses outright (a zero-area ring, a
//! coordinate outside WGS84, a third coordinate, an extension osmium does not
//! read as GeoJSON) fails the check as that tool's error, never as a verdict.
//!
//! The gate reads strictly: a bare Polygon or MultiPolygon, a Feature holding
//! one, or a FeatureCollection holding exactly one such Feature, each ring an
//! array of at least two-number positions. Anything else is refused as a file
//! whose reading by both tools the gate cannot establish. Coordinates are
//! parsed correctly rounded (serde_json's `float_roundtrip`, as pbfhogg parses
//! them), and closure and edge span are judged on the parsed pairs exactly as
//! pbfhogg judges them.

use std::path::Path;

use serde_json::Value;

/// `Ok` when osmium reads the polygon file at `path` as the region pbfhogg
/// reads; otherwise why not, naming the file.
pub(super) fn osmium_reads_alike(path: &Path) -> Result<(), String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read polygon file {}: {e}", path.display()))?;
    let doc: Value = serde_json::from_str(&text)
        .map_err(|e| format!("polygon file {} is not JSON: {e}", path.display()))?;
    judge(&doc).map_err(|why| {
        format!(
            "polygon file {} cannot be compared with osmium: {why}",
            path.display()
        )
    })
}

/// The document's verdict, without the file name.
fn judge(doc: &Value) -> Result<(), String> {
    let geometry = match type_of(doc)? {
        "FeatureCollection" => {
            let features = doc
                .get("features")
                .and_then(Value::as_array)
                .ok_or("a FeatureCollection without a features array")?;
            let [feature] = features.as_slice() else {
                return Err(format!(
                    "a FeatureCollection of {} features (pbfhogg unions every feature, osmium \
                     reads the first); register one Feature",
                    features.len()
                ));
            };
            if type_of(feature)? != "Feature" {
                return Err("a FeatureCollection whose member is not a Feature".into());
            }
            feature_geometry(feature)?
        }
        "Feature" => feature_geometry(doc)?,
        "Polygon" | "MultiPolygon" => doc,
        other => return Err(format!("a GeoJSON {other}, not a polygon")),
    };
    let coordinates = geometry
        .get("coordinates")
        .ok_or("a geometry without coordinates")?;
    let polygons: Vec<&Value> = match type_of(geometry)? {
        "Polygon" => vec![coordinates],
        "MultiPolygon" => coordinates
            .as_array()
            .ok_or("MultiPolygon coordinates that are not an array")?
            .iter()
            .collect(),
        other => return Err(format!("a {other} geometry, not a Polygon or MultiPolygon")),
    };
    for polygon in polygons {
        let rings = polygon
            .as_array()
            .ok_or("polygon coordinates that are not an array of rings")?;
        for ring in rings {
            judge_ring(&positions(ring)?)?;
        }
    }
    Ok(())
}

fn type_of(value: &Value) -> Result<&str, String> {
    value
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| "an object without a string type".to_owned())
}

fn feature_geometry(feature: &Value) -> Result<&Value, String> {
    feature
        .get("geometry")
        .filter(|g| g.is_object())
        .ok_or_else(|| "a Feature without a geometry object".to_owned())
}

/// A ring's `(lon, lat)` pairs.
fn positions(ring: &Value) -> Result<Vec<(f64, f64)>, String> {
    ring.as_array()
        .ok_or("a ring that is not an array")?
        .iter()
        .map(|position| {
            let pair = position
                .as_array()
                .filter(|p| p.len() >= 2)
                .ok_or("a position that is not an array of at least two numbers")?;
            match (pair[0].as_f64(), pair[1].as_f64()) {
                (Some(lon), Some(lat)) => Ok((lon, lat)),
                _ => Err("a position holding something other than numbers".to_owned()),
            }
        })
        .collect()
}

/// A ring both tools read alike: closed (its first and last positions equal,
/// so pbfhogg adds no closing edge), and with no edge, the closing one
/// included, spanning more than 180 degrees of longitude.
fn judge_ring(ring: &[(f64, f64)]) -> Result<(), String> {
    let (Some(first), Some(last)) = (ring.first(), ring.last()) else {
        return Err("an empty ring".into());
    };
    if first != last {
        return Err(format!(
            "an open ring from {first:?} to {last:?} (pbfhogg closes it, osmium uses it as given)"
        ));
    }
    let crosses = ring.windows(2).any(|w| (w[1].0 - w[0].0).abs() > 180.0)
        || (last.0 - first.0).abs() > 180.0;
    if crosses {
        return Err(
            "a ring crossing the antimeridian (pbfhogg unwraps it, osmium spans the globe)".into(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn doc(text: &str) -> Value {
        serde_json::from_str(text).unwrap()
    }

    const SQUARE: &str = "[[[0,0],[1,0],[1,1],[0,1],[0,0]]]";

    #[test]
    fn single_closed_geometries_pass_in_every_wrapper() {
        let polygon = format!(r#"{{"type":"Polygon","coordinates":{SQUARE}}}"#);
        let multi = format!(r#"{{"type":"MultiPolygon","coordinates":[{SQUARE},{SQUARE}]}}"#);
        let feature = format!(r#"{{"type":"Feature","properties":{{}},"geometry":{polygon}}}"#);
        let collection = format!(r#"{{"type":"FeatureCollection","features":[{feature}]}}"#);
        for text in [&polygon, &multi, &feature, &collection] {
            assert_eq!(judge(&doc(text)), Ok(()), "{text}");
        }
        // A hole is one more closed ring of the same polygon.
        let holed = r#"{"type":"Polygon","coordinates":[
            [[0,0],[4,0],[4,4],[0,4],[0,0]],[[1,1],[1,2],[2,2],[1,1]]]}"#;
        assert_eq!(judge(&doc(holed)), Ok(()));
    }

    #[test]
    fn the_readings_pbfhogg_and_osmium_differ_on_are_refused() {
        let polygon = format!(r#"{{"type":"Polygon","coordinates":{SQUARE}}}"#);
        let feature = format!(r#"{{"type":"Feature","properties":{{}},"geometry":{polygon}}}"#);
        let two = format!(r#"{{"type":"FeatureCollection","features":[{feature},{feature}]}}"#);
        assert!(judge(&doc(&two)).unwrap_err().contains("2 features"));

        let open = r#"{"type":"Polygon","coordinates":[[[0,0],[1,0],[1,1],[0,1]]]}"#;
        assert!(judge(&doc(open)).unwrap_err().contains("open ring"));

        let crossing = r#"{"type":"Polygon","coordinates":[
            [[179,0],[-179,0],[-179,1],[179,1],[179,0]]]}"#;
        assert!(judge(&doc(crossing)).unwrap_err().contains("antimeridian"));
    }

    /// A longitude whose correctly rounded value puts an edge just over 180
    /// degrees: read roughly, the span rounds to exactly 180 and the ring
    /// would pass, while pbfhogg, parsing correctly rounded, unwraps it.
    #[test]
    fn edge_spans_are_judged_on_correctly_rounded_coordinates() {
        let rectangle = r#"{"type":"Polygon","coordinates":[[
            [-178,0],[2.0000000000000145,0],[2.0000000000000145,1],[-178,1],[-178,0]]]}"#;
        assert!(judge(&doc(rectangle)).unwrap_err().contains("antimeridian"));
    }

    #[test]
    fn what_the_gate_cannot_read_is_refused_not_passed() {
        for text in [
            r#"{"type":"Point","coordinates":[0,0]}"#,
            r#"{"type":"FeatureCollection","features":[]}"#,
            r#"{"type":"FeatureCollection","features":[{"geometry":{"type":"Polygon","coordinates":[[[0,0],[1,0],[1,1],[0,0]]]}}]}"#,
            r#"{"type":"Feature","geometry":null}"#,
            r#"{"type":"Polygon","coordinates":[[[0,0],[1,"x"],[1,1],[0,0]]]}"#,
            r#"{"type":"Polygon","coordinates":[[]]}"#,
            r#"{"coordinates":[]}"#,
        ] {
            assert!(judge(&doc(text)).is_err(), "{text}");
        }
    }
}
