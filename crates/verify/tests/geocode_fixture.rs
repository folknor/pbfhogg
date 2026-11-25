//! A generated tool protocol driving `verify geocode` end to end.
//!
//! The input is a header-only fixture claiming `Sort.Type_then_ID` (so no
//! osmium sort runs) whose marker names its content, `full` or `empty`. The
//! fake osmium's `tags-filter` writes a fixture whose marker carries both the
//! content and which decoding it is (`full-address`, `empty-admin`, ...), and
//! its `add-locations-to-ways` writes that decoding's OPL, so every decode is
//! a function of the operand's bytes and the cache runs live.
//!
//! The full content: three address nodes (a street with a postcode, a street,
//! a place), a building way placed at the truncated mean of its nodes, a
//! named residential street of three nodes, an excluded footway, no
//! interpolation (the absence-only path) and two closed square boundaries,
//! level 2 with a country code and level 4, both enclosing the whole extent
//! of the modelled data with kilometres of clearance, so both builds judge
//! containing answers at both levels.
//!
//! The fake pbfhogg checks the build argv and writes the admin tolerance into
//! its index directory; its `geocode-query` answers by an independent brute
//! force over the same constants under the documented reader contract (the
//! flat metric, a segment's nearest point, the nearest strictly under the
//! coarse radius, square containment with the area cast through f32). A
//! fault side file, read by the fake pbfhogg alone, aims at one verdict of
//! one build, and each is held to failing exactly that verdict by name.
use std::ffi::OsString;
use std::path::Path;

use crate::{checks, error, fake_pbf, harness, process, reference, reference_cache, run};

const SORTED: &str = "Sort.Type_then_ID";
const ROOT: &str = "geocode-fixture";
const ADDRESS_FILTERS: [&str; 3] = ["nw/addr:housenumber", "w/highway", "w/addr:interpolation"];
const ADMIN_FILTER: &str = "r/boundary=administrative,postal_code";
const EARTH_RADIUS_M: f64 = 6_371_008.8;
const COARSE_M: f64 = 1000.0;

/// `(id, lat, lon, house number, street, place, postcode)`.
type AddressNode = (
    i64,
    i32,
    i32,
    &'static str,
    Option<&'static str>,
    Option<&'static str>,
    Option<&'static str>,
);

const ADDRESSES: [AddressNode; 3] = [
    (
        1,
        550_000_000,
        120_000_000,
        "1",
        Some("Main"),
        None,
        Some("1000"),
    ),
    (2, 550_500_000, 120_800_000, "2", Some("Main"), None, None),
    (3, 549_700_000, 121_500_000, "3", None, Some("Hamlet"), None),
];
/// The building way's nodes, closed by repeating its first node.
const BUILDING: [(i64, i32, i32); 4] = [
    (20, 550_200_000, 121_000_000),
    (21, 550_200_000, 121_000_900),
    (22, 550_200_700, 121_000_900),
    (20, 550_200_000, 121_000_000),
];
const STREET: [(i64, i32, i32); 3] = [
    (11, 549_800_000, 119_900_000),
    (12, 550_300_000, 120_600_000),
    (13, 550_600_000, 121_200_000),
];
const FOOTWAY: [(i64, i32, i32); 2] = [
    (14, 550_100_000, 121_300_000),
    (15, 549_900_000, 121_350_000),
];
/// `(relation, way, first node, level, name, country code, min lat, min
/// lon, max lat, max lon)`.
type Square = (
    i64,
    i64,
    i64,
    u8,
    &'static str,
    Option<&'static str>,
    i32,
    i32,
    i32,
    i32,
);
const SQUARES: [Square; 2] = [
    (
        1,
        100,
        100,
        2,
        "Country",
        Some("DK"),
        548_000_000,
        116_000_000,
        552_000_000,
        126_000_000,
    ),
    (
        2,
        200,
        200,
        4,
        "Region",
        None,
        549_000_000,
        119_000_000,
        551_000_000,
        122_500_000,
    ),
];

fn deg(e7: i32) -> String {
    let sign = if e7 < 0 { "-" } else { "" };
    let v = e7.unsigned_abs();
    format!("{sign}{}.{:07}", v / 10_000_000, v % 10_000_000)
}

fn refs(nodes: &[(i64, i32, i32)]) -> String {
    nodes
        .iter()
        .map(|(id, lat, lon)| format!("n{id}x{}y{}", deg(*lon), deg(*lat)))
        .collect::<Vec<_>>()
        .join(",")
}

fn address_opl() -> String {
    let mut out = String::new();
    for (id, lat, lon, number, street, place, postcode) in ADDRESSES {
        let mut tags = vec![format!("addr:housenumber={number}")];
        tags.extend(street.map(|s| format!("addr:street={s}")));
        tags.extend(place.map(|p| format!("addr:place={p}")));
        tags.extend(postcode.map(|p| format!("addr:postcode={p}")));
        out.push_str(&format!(
            "n{id} v1 T{} x{} y{}\n",
            tags.join(","),
            deg(lon),
            deg(lat)
        ));
    }
    out.push_str(&format!(
        "w10 v1 Thighway=residential,name=Main N{}\n",
        refs(&STREET)
    ));
    out.push_str(&format!(
        "w11 v1 Thighway=footway,name=Path N{}\n",
        refs(&FOOTWAY)
    ));
    out.push_str(&format!(
        "w12 v1 Tbuilding=yes,addr:housenumber=4,addr:street=Main N{}\n",
        refs(&BUILDING)
    ));
    out
}

fn corners(s: &Square) -> [(i64, i32, i32); 5] {
    let (_, _, n, _, _, _, lat0, lon0, lat1, lon1) = *s;
    [
        (n, lat0, lon0),
        (n + 1, lat0, lon1),
        (n + 2, lat1, lon1),
        (n + 3, lat1, lon0),
        (n, lat0, lon0),
    ]
}

fn admin_opl() -> String {
    let mut out = String::new();
    for s in &SQUARES {
        for (id, lat, lon) in &corners(s)[..4] {
            out.push_str(&format!("n{id} v1 T x{} y{}\n", deg(*lon), deg(*lat)));
        }
    }
    for s in &SQUARES {
        out.push_str(&format!("w{} v1 T N{}\n", s.1, refs(&corners(s))));
    }
    for (r, w, _, level, name, code, ..) in SQUARES {
        let code = code.map_or_else(String::new, |c| format!(",ISO3166-1:alpha2={c}"));
        out.push_str(&format!(
            "r{r} v1 Tboundary=administrative,admin_level={level},name={name}{code} Mw{w}@outer\n"
        ));
    }
    out
}

pub fn driver(life: process::Lifecycle, exe: &Path, dir: &Path) {
    let dir = dir.join(ROOT);
    std::fs::create_dir(&dir).unwrap();
    let input = dir.join("input.pbf");
    fake_pbf::write(&input, &[SORTED], "full");
    let run = run::Run::create(&dir, exe, true).unwrap();
    let cache = reference_cache::ReferenceCache::new(&dir, ROOT.into(), false).unwrap();
    let mut h = harness::Harness::new(
        life,
        run,
        reference::Tools { osmium: exe.into() },
        false,
        cache,
    );
    let verdict = |what: &str| format!("geocode {what} vs the osmium-derived model");
    let cases: [(&str, Option<String>, Option<&str>); 8] = [
        ("", None, None),
        ("address", Some(verdict("address answers")), None),
        ("street", Some(verdict("street answers")), None),
        (
            "interpolation",
            Some(verdict("interpolation answers")),
            None,
        ),
        (
            "admin-exact",
            Some(verdict("admin answers at --admin-tolerance 0")),
            None,
        ),
        (
            "admin-band",
            Some(verdict(
                "admin answers outside the band of a non-zero --admin-tolerance",
            )),
            None,
        ),
        (
            "build-exact",
            None,
            Some("pbfhogg build-geocode-index --admin-tolerance 0 failed"),
        ),
        ("short", None, Some("answers for")),
    ];
    for (fault, failed, error) in cases {
        h.run = run::Run::create(&dir, exe, true).unwrap();
        h.run.watch(&input).unwrap();
        std::fs::write(dir.join("fault"), fault).unwrap();
        let name = format!("geocode-{}", if fault.is_empty() { "clean" } else { fault });
        let judged = super::judge(&mut h, &name, |h| checks::geocode::run(h, &input, true));
        match (failed, error) {
            (None, None) => {
                assert_eq!(
                    judged.status,
                    harness::CheckStatus::Passed,
                    "{name}: {judged:?}"
                );
                assert!(judged.failed.is_empty(), "{name}: {judged:?}");
                assert_eq!(judged.error, None, "{name}: {judged:?}");
            }
            (Some(failed), None) => {
                assert_eq!(
                    judged.status,
                    harness::CheckStatus::Failed,
                    "{name}: {judged:?}"
                );
                assert_eq!(judged.failed, [failed], "{name}: {judged:?}");
                assert_eq!(judged.error, None, "{name}: {judged:?}");
            }
            (_, Some(error)) => {
                assert_eq!(
                    judged.status,
                    harness::CheckStatus::Failed,
                    "{name}: {judged:?}"
                );
                assert!(judged.failed.is_empty(), "{name}: {judged:?}");
                let got = judged.error.as_deref().unwrap_or_default();
                assert!(got.contains(error), "{name}: {judged:?}");
            }
        }
    }
    // A decoding with no address point and no street leaves nothing to
    // draw queries over: a verifier error, never a PASS that did not look.
    std::fs::write(dir.join("fault"), "").unwrap();
    let empty = dir.join("empty.pbf");
    fake_pbf::write(&empty, &[SORTED], "empty");
    h.run = run::Run::create(&dir, exe, true).unwrap();
    h.run.watch(&empty).unwrap();
    let result = h.run_check("geocode-empty", |h| checks::geocode::run(h, &empty, true));
    assert!(
        matches!(result, Err(error::Error::Tool(ref why)) if why.contains("no address point and no street")),
        "{result:?}"
    );
}

pub fn tool(args: &[OsString]) -> i32 {
    let args: Vec<_> = args.iter().map(|s| s.to_str().unwrap()).collect();
    let value = |flag: &str| args[args.iter().position(|s| *s == flag).unwrap() + 1];
    let root = args
        .iter()
        .map(Path::new)
        .find_map(|p| {
            p.ancestors()
                .find(|a| a.file_name().is_some_and(|n| n == ROOT))
        })
        .unwrap()
        .to_path_buf();
    let fault = std::fs::read_to_string(root.join("fault")).unwrap_or_default();
    match args[0] {
        "tags-filter" => {
            let content = fake_pbf::marker(Path::new(args[1]));
            let kind = if args.contains(&ADMIN_FILTER) {
                assert_eq!(&args[2..3], [ADMIN_FILTER], "{args:?}");
                "admin"
            } else {
                assert_eq!(args[2..5], ADDRESS_FILTERS, "{args:?}");
                "address"
            };
            fake_pbf::write(
                Path::new(value("-o")),
                &[SORTED],
                &format!("{content}-{kind}"),
            );
            0
        }
        "add-locations-to-ways" => {
            assert!(args.contains(&"--ignore-missing-nodes"), "{args:?}");
            assert_eq!(value("-f"), "opl", "{args:?}");
            let input = args.iter().find(|a| a.ends_with(".osm.pbf")).unwrap();
            let marker = fake_pbf::marker(Path::new(input));
            let admin = marker.ends_with("-admin");
            assert_eq!(args.contains(&"--keep-untagged-nodes"), admin, "{args:?}");
            let opl = match marker.as_str() {
                "full-address" => address_opl(),
                "full-admin" => admin_opl(),
                _ => String::new(),
            };
            std::fs::write(value("-o"), opl).unwrap();
            0
        }
        "build-geocode-index" => {
            for flag in ["--direct-io", "--staging-dir", "--tmp-dir"] {
                assert!(args.contains(&flag), "{args:?}");
            }
            assert_eq!(value("--search-radius"), "75");
            assert_eq!(value("--coarse-search-radius"), "1000");
            let tolerance = value("--admin-tolerance");
            if fault == "build-exact" && tolerance == "0" {
                eprintln!("fixture build refusal");
                return 1;
            }
            let index = Path::new(value("--output-dir"));
            std::fs::create_dir_all(index).unwrap();
            std::fs::write(index.join("tolerance"), tolerance).unwrap();
            0
        }
        "geocode-query" => {
            let exact =
                std::fs::read_to_string(Path::new(args[1]).join("tolerance")).unwrap() == "0";
            let points = std::fs::read_to_string(args[2]).unwrap();
            let mut lines: Vec<String> = points
                .lines()
                .map(|line| {
                    let (lat, lon) = line.split_once(' ').unwrap();
                    answer(lat.parse().unwrap(), lon.parse().unwrap(), exact, &fault).to_string()
                })
                .collect();
            if fault == "short" && !exact {
                lines.pop();
            }
            for line in lines {
                println!("{line}");
            }
            0
        }
        _ => panic!("unexpected geocode fixture invocation: {args:?}"),
    }
}

fn rad(e7: i32) -> f64 {
    (f64::from(e7) * 1e-7).to_radians()
}

/// The reader's flat-metric distance of `(lat, lon)` (e7) from the query,
/// and of the nearest point on a segment, with its parameter.
fn segment(q: (f64, f64), a: (i32, i32), b: (i32, i32)) -> (f64, f64) {
    let (qlat, qlon) = (q.0.to_radians(), q.1.to_radians());
    let cos = qlat.cos();
    let (ax, ay) = ((rad(a.1) - qlon) * cos, rad(a.0) - qlat);
    let (dx, dy) = ((rad(b.1) - rad(a.1)) * cos, rad(b.0) - rad(a.0));
    let len = dx * dx + dy * dy;
    let t = if len == 0.0 {
        0.0
    } else {
        (-(ax * dx + ay * dy) / len).clamp(0.0, 1.0)
    };
    (
        t,
        ((ax + t * dx).powi(2) + (ay + t * dy).powi(2)).sqrt() * EARTH_RADIUS_M,
    )
}

fn answer(lat: f64, lon: f64, exact: bool, fault: &str) -> serde_json::Value {
    use serde_json::json;
    let q = (lat, lon);
    let mut addresses: Vec<AddressNode> = ADDRESSES.to_vec();
    let sum = BUILDING.iter().fold((0_i64, 0_i64), |s, n| {
        (s.0 + i64::from(n.1), s.1 + i64::from(n.2))
    });
    let count = i64::try_from(BUILDING.len()).unwrap();
    // The building's address sits at the truncated mean of its nodes, the
    // repeated closing node counted again.
    addresses.push((
        12,
        i32::try_from(sum.0 / count).unwrap(),
        i32::try_from(sum.1 / count).unwrap(),
        "4",
        Some("Main"),
        None,
        None,
    ));
    let address = addresses
        .iter()
        .map(|a| (segment(q, (a.1, a.2), (a.1, a.2)).1, a))
        .min_by(|x, y| x.0.total_cmp(&y.0))
        .filter(|(d, _)| *d < COARSE_M)
        .map_or(serde_json::Value::Null, |(d, a)| {
            let number = if fault == "address" && !exact {
                "9"
            } else {
                a.3
            };
            json!({"lat_e7": a.1, "lon_e7": a.2, "house_number": number, "street": a.4,
                   "place": a.5, "postcode": a.6, "distance_m": d})
        });
    let street = STREET
        .windows(2)
        .map(|w| {
            let (a, b) = ((w[0].1, w[0].2), (w[1].1, w[1].2));
            let (t, d) = segment(q, a, b);
            #[allow(clippy::cast_possible_truncation)]
            let along = |a: i32, b: i32| (f64::from(a) + t * (f64::from(b) - f64::from(a))) as i64;
            (d, along(a.0, b.0), along(a.1, b.1))
        })
        .min_by(|x, y| x.0.total_cmp(&y.0))
        .filter(|s| s.0 < COARSE_M)
        .map_or(serde_json::Value::Null, |(d, lat, lon)| {
            let name = if fault == "street" && !exact {
                "Wrong"
            } else {
                "Main"
            };
            json!({"name": name, "snap_lat_e7": lat, "snap_lon_e7": lon, "distance_m": d})
        });
    let interpolation = if fault == "interpolation" && !exact {
        json!({"street": "Main", "place": null, "house_number": 1, "distance_m": 1.0})
    } else {
        serde_json::Value::Null
    };
    let (y, x) = (lat * 1e7, lon * 1e7);
    let admin: Vec<serde_json::Value> = if fault == "admin-exact" && exact {
        Vec::new()
    } else {
        SQUARES
            .iter()
            .filter(|s| {
                y > f64::from(s.6) && y < f64::from(s.8) && x > f64::from(s.7) && x < f64::from(s.9)
            })
            .map(|s| {
                let side = i64::from(s.8 - s.6) * i64::from(s.9 - s.7);
                #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
                let mut area = f64::from(side as f32);
                if fault == "admin-band" && !exact {
                    area *= 2.0;
                }
                json!({"admin_level": s.3, "name": s.4, "country_code": s.5, "area": area})
            })
            .collect()
    };
    json!({"lat": lat, "lon": lon, "address": address, "street": street,
           "interpolation": interpolation, "admin": admin})
}
