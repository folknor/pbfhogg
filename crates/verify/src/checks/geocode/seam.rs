//! geocode-seam: the geocode check run on a fixed fixture straddling the
//! antimeridian, with a cluster past 85 degrees of latitude, so the seam
//! and the high latitudes are judged on every suite run although no
//! configured dataset reaches them.
//!
//! The fixture is written as OPL, converted by osmium (unindexed) and
//! rewritten by pbfhogg cat (indexed), as check-ids builds its fixtures, and
//! then judged exactly as `geocode` judges an input: two builds, osmium
//! decodings, the model, the sampled queries and every verdict. Named
//! queries follow the sampled ones. Each names a seam rule, and the model's
//! own expectation there is held to what the fixture was drawn to show
//! before pbfhogg's answer is judged like any other: a named case the model
//! does not expect as drawn is a verifier error, never a pass.
//!
//! No fixture test drives this run with fake tools, by design: it judges
//! real builds of a real file, and its model is the `geocode` model, whose
//! fake-tool fixture covers the orchestration.

use crate::error::Error;
use crate::harness::Harness;

use crate::harness::Verdict;
use crate::tunables::GEOCODE_DISTANCE_TOLERANCE_M as TOL;

use super::admin::{AdminExpect, AdminModel, Level};
use super::{Expect, Query, StreetAnswer, judge, judge_input};

/// The fixture. Coordinates are `x` longitude, `y` latitude.
const FIXTURE: &str = "\
n1 v1 x179.999 y-17.0 Taddr:housenumber=1,addr:street=SeamRoad
n2 v1 x-179.999 y-17.0005 Taddr:housenumber=2,addr:street=SeamRoad
n4 v1 x-180.0 y-17.006 Taddr:housenumber=4,addr:street=SeamRoad
n20 v1 x180.0 y-17.008 T
n21 v1 x179.995 y-17.008 T
n22 v1 x-180.0 y-17.008 Taddr:housenumber=10,addr:street=Range
n23 v1 x179.995 y-17.008 Taddr:housenumber=20,addr:street=Range
n24 v1 x-179.995 y-17.008 T
n25 v1 x-179.995 y-17.008 Taddr:housenumber=30,addr:street=Range
n30 v1 x179.9995 y-17.003 T
n31 v1 x-179.9995 y-17.003 T
n32 v1 x-179.9995 y-17.0032 T
n33 v1 x179.9995 y-17.0032 T
n40 v1 x179.997 y-17.0015 T
n41 v1 x-179.997 y-17.0015 T
n42 v1 x-180.0 y-17.01 T
n43 v1 x-180.0 y-17.011 T
n50 v1 x20.0 y86.0 Taddr:housenumber=1,addr:street=Polar
n51 v1 x20.002 y86.0005 Taddr:housenumber=2,addr:street=Polar
n52 v1 x20.0 y86.0 T
n53 v1 x20.01 y86.001 T
n60 v1 x179.98 y-17.02 T
n61 v1 x-179.98 y-17.02 T
n62 v1 x-179.98 y-16.98 T
n63 v1 x179.98 y-16.98 T
n64 v1 x179.999 y-17.0105 T
n65 v1 x-179.999 y-17.0105 T
n66 v1 x-179.999 y-17.0095 T
n67 v1 x179.999 y-17.0095 T
n70 v1 x-180.0 y-17.02 T
n71 v1 x-179.99 y-17.02 T
n72 v1 x-179.99 y-17.0 T
n73 v1 x-180.0 y-17.0 T
n74 v1 x179.985 y-17.019 T
n75 v1 x-179.995 y-17.019 T
n76 v1 x-179.995 y-16.99 T
n77 v1 x179.985 y-16.99 T
n80 v1 x0.0 y30.0 T
n81 v1 x180.0 y40.0 T
n82 v1 x0.0 y45.0 T
n83 v1 x-180.0 y55.0 T
n84 v1 x180.0 y72.0 T
n85 v1 x0.0 y62.0 T
w120 v1 Taddr:interpolation=even,addr:street=Range Nn21,n20,n24
w121 v1 Tbuilding=yes,addr:housenumber=5,addr:street=SeamRoad Nn30,n31,n32,n33,n30
w122 v1 Thighway=residential,name=SeamRoad Nn40,n41
w123 v1 Thighway=residential,name=Meridian Nn42,n43
w124 v1 Thighway=residential,name=Polar Nn52,n53
w130 v1 T Nn60,n61,n62,n63,n60
w131 v1 T Nn64,n65,n66,n67,n64
w132 v1 T Nn70,n71,n72,n73,n70
w133 v1 T Nn74,n75,n76,n77,n74
w140 v1 Thighway=residential,name=HalfPlus Nn80,n81
w141 v1 Thighway=residential,name=HalfMinus Nn82,n83
w142 v1 Thighway=residential,name=HalfReversed Nn84,n85
r1 v1 Tboundary=administrative,admin_level=2,name=SeamCountry,ISO3166-1:alpha2=FJ Mw130@outer,w131@inner
r2 v1 Tboundary=administrative,admin_level=4,name=WestEdge Mw132@outer
r3 v1 Tboundary=administrative,admin_level=4,name=BigFour Mw133@outer
";

/// What a named query's expectations must show.
type Rule = fn(&Query, &AdminModel, [&AdminExpect; 2]) -> Result<(), String>;

/// The named queries: what each shows, where, and the rule its model
/// expectation is held to.
const NAMED: [(&str, f64, f64, Rule); 11] = [
    (
        "an address stored at -180 answers a query at +180",
        -17.006,
        179.9998,
        |q, _, _| {
            let near = nearest(&q.address, "address")?;
            (near.lon == -1_800_000_000 && near.house_number == "4")
                .then_some(())
                .ok_or_else(|| format!("nearest {near:?}"))
        },
    ),
    (
        "a range resolves through both spellings of the antimeridian",
        -17.0081,
        179.9975,
        |q, _, _| nearest(&q.interpolation, "interpolation").map(drop),
    ),
    (
        "a range across the antimeridian numbers its far side by length across it",
        -17.0081,
        -179.9975,
        |q, _, _| {
            let near = nearest(&q.interpolation, "interpolation")?;
            near.allows(20)
                .then_some(())
                .ok_or_else(|| format!("nearest {near:?}"))
        },
    ),
    (
        "a street past 85 degrees of latitude answers",
        86.0005,
        20.005,
        |q, _, _| {
            let near = nearest(&q.street, "street")?;
            (near.name == "Polar")
                .then_some(())
                .ok_or_else(|| format!("nearest {near:?}"))
        },
    ),
    (
        "a snap on the antimeridian is spelled +180",
        -17.0105,
        179.9999,
        |q, _, _| {
            let near = nearest(&q.street, "street")?;
            (near.name == "Meridian" && near.snap_lon == 1_800_000_000)
                .then_some(())
                .ok_or_else(|| format!("nearest {near:?}"))
        },
    ),
    (
        "a street across the antimeridian answers its far side",
        -17.0016,
        -179.999,
        |q, _, _| {
            let near = nearest(&q.street, "street")?;
            (near.name == "SeamRoad")
                .then_some(())
                .ok_or_else(|| format!("nearest {near:?}"))
        },
    ),
    (
        "a hole across the antimeridian removes the country",
        -17.01,
        -179.9995,
        |_, _, bands| each(bands, 2, |l| matches!(l, Level::None { in_hole: true, .. })),
    ),
    (
        "a country across the antimeridian contains its far side",
        -17.015,
        -179.985,
        |_, admin, bands| {
            each(bands, 2, |l| match l {
                Level::Some { candidates, .. } => {
                    candidates.len() == 1 && admin.polygons[candidates[0]].name == "SeamCountry"
                }
                _ => false,
            })
        },
    ),
    (
        "a ring touching the antimeridian leaves -180 on its band",
        -17.015,
        -180.0,
        |q, admin, bands| on_west_edge(q, admin, bands),
    ),
    (
        "a ring touching the antimeridian leaves +180 on its band",
        -17.015,
        180.0,
        |q, admin, bands| on_west_edge(q, admin, bands),
    ),
    (
        "two areas of one level overlap across the antimeridian, the smaller certified",
        -17.015,
        -179.997,
        |_, admin, bands| {
            each(bands, 4, |l| match l {
                Level::Some { candidates, .. } => {
                    let named = |n: &str| candidates.iter().find(|&&i| admin.polygons[i].name == n);
                    match (named("WestEdge"), named("BigFour")) {
                        (Some(&w), Some(&b)) => {
                            candidates.len() == 2
                                && admin.polygons[w].area.1 < admin.polygons[b].area.0
                        }
                        _ => false,
                    }
                }
                _ => false,
            })
        },
    ),
];

/// The half-turn path queries: what each shows, where, the street, whether
/// the two half-world paths tie there, and whether the answer's snap must
/// lie east of the meridian (positive longitude).
///
/// Each street spans exactly half a turn from a node at longitude 0, the
/// lesser endpoint by `(longitude, latitude)` with -180 ordered as +180, so
/// the reader projects both paths from longitude 0. A query on that meridian
/// is then an exact mirror of itself across it: both paths' squared
/// distances are bit-equal in the reader, whichever order the way's nodes
/// run and however the far end spells the antimeridian, and the rule that
/// the path east of the lesser endpoint wins is the only thing deciding the
/// snap. The model ties the two paths within its tolerance and cannot tell
/// an exact tie from a near one in general, so the path is judged only here,
/// where the tie is exact by construction. The control just west of the
/// meridian is no tie: the west path is nearer by about a millimetre.
const TIES: [(&str, f64, f64, &str, bool, bool); 4] = [
    (
        "an exact half-turn tie takes the path east of the lesser endpoint",
        30.005,
        0.0,
        "HalfPlus",
        true,
        true,
    ),
    (
        "the tie does not follow the far end's spelling of the antimeridian",
        45.005,
        0.0,
        "HalfMinus",
        true,
        true,
    ),
    (
        "the tie does not follow the order of the way's nodes",
        62.005,
        0.0,
        "HalfReversed",
        true,
        true,
    ),
    (
        "the strictly nearer half-world path wins over the tie preference",
        30.005,
        -0.000_000_1,
        "HalfPlus",
        false,
        false,
    ),
];

/// The model's candidates at a `TIES` query: two of `name` at one distance
/// with snaps either side of the meridian at a tie, one west of it
/// otherwise. Returns the candidate the answer must be.
fn tie_candidate(
    q: &Query,
    name: &str,
    tie: bool,
    east: bool,
) -> Result<(bool, (f64, StreetAnswer)), String> {
    let Expect::Some(fine, tied) = &q.street else {
        return Err(format!("street expected {:?}", q.street));
    };
    let ok = tied.iter().all(|(_, s)| s.name == name && s.snap_lon != 0)
        && if tie {
            tied.len() == 2
                && (tied[0].0 - tied[1].0).abs() <= TOL
                && (tied[0].1.snap_lon > 0) != (tied[1].1.snap_lon > 0)
        } else {
            tied.len() == 1
        };
    let pick = tied.iter().find(|(_, s)| (s.snap_lon > 0) == east);
    match pick {
        Some(c) if ok => Ok((*fine, c.clone())),
        _ => Err(format!("street candidates {tied:?}")),
    }
}

/// Level 4 unjudged at both bands because the `WestEdge` ring, which does
/// not unwrap, has its edge on the antimeridian within each band.
fn on_west_edge(q: &Query, admin: &AdminModel, bands: [&AdminExpect; 2]) -> Result<(), String> {
    each(bands, 4, |l| matches!(l, Level::Unjudged))?;
    for band in [
        crate::tunables::GEOCODE_ADMIN_EXACT_BAND_M,
        super::admin::TOLERANCE_BAND_M,
    ] {
        let near = admin.near_edges(q.lat, q.lon, band);
        if !near
            .iter()
            .any(|&p| admin.polygons[p as usize].name == "WestEdge")
        {
            return Err(format!("no WestEdge edge within {band} m"));
        }
    }
    Ok(())
}

/// The nearest expected candidate of a class, which must be judged.
fn nearest<'e, T>(expect: &'e Expect<T>, class: &str) -> Result<&'e T, String> {
    match expect {
        Expect::Some(_, tied) => tied
            .iter()
            .min_by(|a, b| a.0.total_cmp(&b.0))
            .map(|t| &t.1)
            .ok_or_else(|| format!("{class}: no candidate")),
        _ => Err(format!("{class}: expected nothing or not judged")),
    }
}

/// `holds` at `level` in both builds' expectations.
fn each(
    bands: [&AdminExpect; 2],
    level: usize,
    holds: impl Fn(&Level) -> bool,
) -> Result<(), String> {
    for (band, e) in ["tolerance 0", "non-zero tolerance"].iter().zip(bands) {
        if !holds(&e.levels[level]) {
            return Err(format!("level {level} at {band}: {:?}", e.levels[level]));
        }
    }
    Ok(())
}

pub fn run(harness: &mut Harness, direct_io: bool) -> Result<Option<String>, Error> {
    let outdir = harness.subdir("geocode-seam")?;
    harness.msg(
        "=== verify geocode-seam (the geocode check on a fixture across the antimeridian) ===",
    );
    let (_, fixture) = crate::checks::check_ids::build_fixture(
        harness,
        &outdir,
        "seam",
        FIXTURE,
        "geocode-seam-fixture.1",
    )?;
    harness.run.watch(&fixture)?;
    let points: Vec<(f64, f64)> = NAMED
        .iter()
        .map(|n| (n.1, n.2))
        .chain(TIES.iter().map(|t| (t.1, t.2)))
        .collect();
    let judged = judge_input(harness, &fixture, &outdir, direct_io, &points)?;
    let first = judged.queries.len() - points.len();
    for (k, (what, _, _, rule)) in NAMED.iter().enumerate() {
        let i = first + k;
        let bands = [&judged.admin_expects[0][i], &judged.admin_expects[1][i]];
        rule(&judged.queries[i], &judged.admin, bands).map_err(|why| {
            Error::tool(format!(
                "named query {} ({what}): the model does not expect what the fixture shows: {why}",
                judged.queries[i].text
            ))
        })?;
        harness.msg(&format!("  named {}: {what}", judged.queries[i].text));
    }
    let mut wrong = Vec::new();
    for (k, (what, _, _, name, tie, east)) in TIES.iter().enumerate() {
        let i = first + NAMED.len() + k;
        let q = &judged.queries[i];
        let (fine, want) = tie_candidate(q, name, *tie, *east).map_err(|why| {
            Error::tool(format!(
                "named query {} ({what}): the model does not expect what the fixture shows: {why}",
                q.text
            ))
        })?;
        let street = judged.answered[i]
            .get("street")
            .ok_or_else(|| "the answer has no street key".to_owned());
        if let Err(why) = street.and_then(|v| judge(&Expect::Some(fine, vec![want]), v)) {
            wrong.push(format!("query {} ({what}): {why}", q.text));
        }
        harness.msg(&format!("  named {}: {what}", q.text));
    }
    for why in &wrong {
        harness.msg(&format!("    {why}"));
    }
    harness.record(
        "geocode half-turn street answers take the reader's path",
        Verdict::from_pass(wrong.is_empty()),
    );
    Ok(Some(format!(
        "{} queries over a fixture across the antimeridian and past 85 degrees of latitude, \
         {} named seam cases expected as drawn and judged like the rest, {} half-turn \
         path cases judged on the path",
        judged.queries.len(),
        NAMED.len(),
        TIES.len()
    )))
}
