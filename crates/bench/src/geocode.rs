//! `geocode-build` and `geocode-query`: the geocode-query bench's untimed
//! setup and its measured instrument.

use pbfhogg::geocode_index::reader::{QueryError, Reader, ReverseResult};
use pbfhogg::host::Host;
use std::path::Path;
use std::time::Instant;

/// Build the index the query instrument measures, from `input` into
/// `output_dir`, the run scratch under `scratch_parent`. The builder's
/// defaults throughout, as a bare `build-geocode-index --force` gets them:
/// an existing index is replaced and an unindexed input accepted (the
/// bench's inputs are its own to choose), reads buffered. The full builder
/// runs, publication and smoke test included.
pub(crate) fn build(
    input: &Path,
    output_dir: &Path,
    scratch_parent: &Path,
    host: &Host,
) -> pbfhogg::Result<()> {
    use pbfhogg::geocode_index::builder::{
        BuildConfig, build_geocode_index, index_output_with_staging,
    };
    let output = index_output_with_staging(
        output_dir,
        pbfhogg::Overwrite::Replace,
        pbfhogg::Staging::Beside,
        &host.diagnostics,
    )?;
    let scratch = pbfhogg::scratch::RunScratch::create(scratch_parent, host)?;
    let config = BuildConfig {
        allow_unindexed: true,
        direct_io: false,
        ..BuildConfig::default()
    };
    build_geocode_index(input, &config, output, host, scratch)?;
    Ok(())
}

/// Time `Reader::query` over a fixed pseudo-random point set. The points are
/// a pure function of bbox and seed (splitmix64, uniform in the box), so two
/// revisions query the same points. One untimed warm-up round faults the
/// index into the page cache; each timed round then queries every point
/// once. Reports the timed rounds' total as `elapsed_ms` and each round's
/// per-query mean. The answers are not reported: an instrument measures,
/// and whether the reader answers correctly is the geocode tests' question.
///
/// The warm-up and the timed rounds are each a marker pair on the FIFO,
/// `GEOCODE_QUERY_WARMUP` and `GEOCODE_QUERY_ROUNDS`, so a bench can time
/// or stop at either.
pub(crate) fn query(
    dir: &Path,
    workload: &Workload,
    host: &Host,
    markers: crate::host::Markers<'_>,
) -> Result<(), crate::Failure> {
    let Workload {
        bbox,
        queries,
        seed,
        rounds,
    } = *workload;
    if queries == 0 || rounds == 0 {
        return Err(crate::Failure::Message(
            "geocode-query needs --queries and --rounds above 0".to_owned(),
        ));
    }
    let points = points(&bbox, queries, seed);

    let open_start = Instant::now();
    let reader = Reader::open(dir).map_err(|e| crate::Failure::Message(e.to_string()))?;
    let open_ms = open_start.elapsed().as_millis();

    let Passes {
        round_ns,
        elapsed_ms,
    } = passes(
        |lat, lon| reader.query(lat, lon),
        || host.invocation.is_cancelled(),
        markers,
        &points,
        rounds,
    )?;

    #[allow(clippy::cast_precision_loss, reason = "a mean in microseconds")]
    let per_query = |ns: u128| ns as f64 / f64::from(queries) / 1000.0;
    let rounds_us: Vec<String> = round_ns
        .iter()
        .map(|&ns| format!("{:.2}", per_query(ns)))
        .collect();
    let best = round_ns.iter().copied().min().unwrap_or(0);
    eprintln!("elapsed_ms={elapsed_ms}");
    eprintln!("open_ms={open_ms}");
    eprintln!("queries={queries}");
    eprintln!("rounds={rounds}");
    eprintln!("us_per_query_best={:.2}", per_query(best));
    eprintln!("us_per_query_rounds={}", rounds_us.join(","));
    Ok(())
}

/// What geocode-query queries.
#[derive(Clone, Copy)]
pub(crate) struct Workload {
    pub(crate) bbox: pbfhogg::extract::Bbox,
    pub(crate) queries: u32,
    pub(crate) seed: u64,
    pub(crate) rounds: u32,
}

/// What the warm-up and timed passes measured.
struct Passes {
    round_ns: Vec<u128>,
    elapsed_ms: u128,
}

/// The warm-up pass, then `rounds` timed
/// passes over `points`. Any query error, in either phase, fails the whole
/// run: a failed query does less work than an answered one, so a revision
/// whose queries fail must never read as faster. The validated bbox and the
/// clamp in [`points`] keep every point in range, so with today's reader an
/// error means the reader changed, not the workload. `query` is a closure
/// so a test can inject a deliberate error.
///
/// `cancelled` is asked before and after the warm-up, before each timed
/// round and after the last: a query never blocks, so a phase or round
/// boundary is where a SIGTERM (brokkr's stop or `brokkr kill`) ends the
/// run, with the ordinary return path and so the profiler's report.
fn passes<'a, F>(
    mut query: F,
    cancelled: impl Fn() -> bool,
    markers: crate::host::Markers<'_>,
    points: &[(f64, f64)],
    rounds: u32,
) -> Result<Passes, crate::Failure>
where
    F: FnMut(f64, f64) -> Result<ReverseResult<'a>, QueryError>,
{
    let failed = |phase: &str, i: usize, e: QueryError| {
        let (lat, lon) = points[i];
        crate::Failure::Message(format!(
            "geocode-query: {phase}, point {i} (lat {lat}, lon {lon}) failed: {e}"
        ))
    };
    // An `_END` marker means its phase completed: a failed or cancelled
    // phase emits only its `_START`.
    markers.marker("GEOCODE_QUERY_WARMUP_START");
    if cancelled() {
        return Err(crate::Failure::Cancelled);
    }

    // Every pass keeps only a sink against the optimizer.
    let mut sink = 0usize;
    for (i, &(lat, lon)) in points.iter().enumerate() {
        let r = query(lat, lon).map_err(|e| failed("warm-up", i, e))?;
        sink = sink.wrapping_add(r.admin.len());
    }
    markers.marker("GEOCODE_QUERY_WARMUP_END");
    if cancelled() {
        return Err(crate::Failure::Cancelled);
    }

    // Grown per round, never reserved from `rounds`: a large round count
    // must not allocate before the first cancellation check of a round.
    let mut round_ns: Vec<u128> = Vec::new();
    markers.marker("GEOCODE_QUERY_ROUNDS_START");
    let timed_start = Instant::now();
    // loop-ends: a counted round of queries, timed; nothing here waits
    for round in 0..rounds {
        if cancelled() {
            return Err(crate::Failure::Cancelled);
        }
        let start = Instant::now();
        for (i, &(lat, lon)) in points.iter().enumerate() {
            match query(lat, lon) {
                Ok(r) => sink = sink.wrapping_add(r.admin.len()),
                Err(e) => return Err(failed(&format!("timed round {}", round + 1), i, e)),
            }
        }
        round_ns.push(start.elapsed().as_nanos());
    }
    let elapsed_ms = timed_start.elapsed().as_millis();
    // A cancellation taken during the last round ends the run too: a run
    // the signal reached never reports a measurement.
    if cancelled() {
        return Err(crate::Failure::Cancelled);
    }
    markers.marker("GEOCODE_QUERY_ROUNDS_END");
    std::hint::black_box(sink);
    Ok(Passes {
        round_ns,
        elapsed_ms,
    })
}

/// The sampling box as typed: four f64 edges validated for WGS84 range and
/// strict order, never quantized. `pbfhogg::extract::parse_bbox` rounds each
/// edge to a decimicrodegree because that is extraction's membership rule;
/// a sampling box is not a membership test, and quantizing it would collapse
/// a box finer than one column to a single point outside what was asked.
pub(crate) fn parse_sampling_bbox(text: &str) -> Result<pbfhogg::extract::Bbox, String> {
    let parts: Vec<&str> = text.split(',').collect();
    let [min_lon, min_lat, max_lon, max_lat] = parts.as_slice() else {
        return Err(format!(
            "bbox needs four comma-separated values, got {}",
            parts.len()
        ));
    };
    let edge = |s: &str| {
        s.trim()
            .parse::<f64>()
            .map_err(|_| format!("invalid bbox value: {s}"))
    };
    pbfhogg::extract::Bbox::try_new(
        edge(min_lon)?,
        edge(min_lat)?,
        edge(max_lon)?,
        edge(max_lat)?,
    )
    .map_err(|e| e.to_string())
}

/// `queries` (lat, lon) points uniform in `bbox`, a pure function of bbox
/// and seed (splitmix64). Each coordinate is clamped to its bbox interval,
/// since the scaling can round an ulp past the upper bound, and at a world
/// bound that point would be out of range; the clamp moves only such edge
/// points, so revisions query the same points. Before registry version 5
/// the box was quantized to decimicrodegrees first, so the points agree
/// across that change only for a box whose edges all lie on the `1e-7`
/// grid.
#[allow(
    clippy::cast_precision_loss,
    reason = "the top 53 bits are exact in an f64"
)]
fn points(bbox: &pbfhogg::extract::Bbox, queries: u32, seed: u64) -> Vec<(f64, f64)> {
    let mut state = seed;
    let mut unit = || {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        // The top 53 bits, exact in an f64, scaled into [0, 1).
        (z >> 11) as f64 / (1u64 << 53) as f64
    };
    (0..queries)
        .map(|_| {
            let lat = bbox.min_lat + unit() * (bbox.max_lat - bbox.min_lat);
            let lon = bbox.min_lon + unit() * (bbox.max_lon - bbox.min_lon);
            (
                lat.clamp(bbox.min_lat, bbox.max_lat),
                lon.clamp(bbox.min_lon, bbox.max_lon),
            )
        })
        .collect()
}

/// The instrument refuses a query error in either phase rather than timing
/// less work, and its points stay inside the bbox.
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::{passes, points};
    use pbfhogg::geocode_index::reader::{QueryError, ReverseResult};

    fn empty() -> ReverseResult<'static> {
        ReverseResult {
            address: None,
            street: None,
            interpolation: None,
            admin: Vec::new(),
        }
    }

    /// A query that answers its first `ok` calls and fails from then on.
    fn failing_after(
        ok: usize,
    ) -> impl FnMut(f64, f64) -> Result<ReverseResult<'static>, QueryError> {
        let mut calls = 0;
        move |lat, _| {
            calls += 1;
            if calls > ok {
                Err(QueryError::LatitudeOutOfRange(lat))
            } else {
                Ok(empty())
            }
        }
    }

    const POINTS: [(f64, f64); 3] = [(1.0, 2.0), (3.0, 4.0), (5.0, 6.0)];
    const NONE: crate::host::Markers<'static> = crate::host::Markers(None);

    fn message(failure: crate::Failure) -> String {
        match failure {
            crate::Failure::Message(message) => message,
            crate::Failure::Library(e) => panic!("a library error: {e}"),
            crate::Failure::Cancelled => panic!("cancelled"),
        }
    }

    #[test]
    fn every_query_answered_succeeds() {
        let Ok(passes) = passes(failing_after(usize::MAX), || false, NONE, &POINTS, 2) else {
            panic!("every query answered");
        };
        assert_eq!(passes.round_ns.len(), 2);
    }

    #[test]
    fn a_warm_up_error_fails_the_run() {
        let Err(e) = passes(failing_after(1), || false, NONE, &POINTS, 2) else {
            panic!("a warm-up error must fail the run");
        };
        let e = message(e);
        assert!(e.contains("warm-up, point 1"), "{e}");
    }

    #[test]
    fn an_error_in_a_later_timed_round_fails_the_run() {
        // Warm-up and the first timed round answer; round 2 fails at point 2.
        let Err(e) = passes(
            failing_after(POINTS.len() * 2 + 2),
            || false,
            NONE,
            &POINTS,
            3,
        ) else {
            panic!("a timed-round error must fail the run");
        };
        let e = message(e);
        assert!(e.contains("timed round 2, point 2"), "{e}");
    }

    /// A cancellation ends the passes at the next round boundary, as a
    /// cancellation, never as a completed run.
    #[test]
    fn a_cancellation_ends_the_passes_at_a_round_boundary() {
        let asked = std::cell::Cell::new(0);
        // Asked before the warm-up, after it, then before round 1 and round
        // 2; cancelled from the fourth ask, before round 2.
        let cancelled = || {
            asked.set(asked.get() + 1);
            asked.get() > 3
        };
        let result = passes(failing_after(usize::MAX), cancelled, NONE, &POINTS, 5);
        assert!(matches!(result, Err(crate::Failure::Cancelled)));
        assert_eq!(asked.get(), 4);
    }

    /// A cancellation taken during the last round still ends the run as a
    /// cancellation, never as a completed measurement.
    #[test]
    fn a_cancellation_during_the_last_round_is_not_a_measurement() {
        let tripped = std::cell::Cell::new(false);
        let calls = std::cell::Cell::new(0);
        // Warm-up and round 1 answer untouched; the signal lands while the
        // last round (round 2) is querying.
        let query = |_, _| {
            calls.set(calls.get() + 1);
            if calls.get() > POINTS.len() * 2 {
                tripped.set(true);
            }
            Ok(empty())
        };
        let result = passes(query, || tripped.get(), NONE, &POINTS, 2);
        assert!(matches!(result, Err(crate::Failure::Cancelled)));
        assert_eq!(calls.get(), POINTS.len() * 3);
    }

    /// A box finer than one decimicrodegree is sampled as typed, not
    /// collapsed onto the column its edges round to.
    #[test]
    fn a_sub_column_box_is_not_quantized() {
        let bbox =
            super::parse_sampling_bbox("0.00000001,0.00000001,0.00000002,0.00000002").unwrap();
        assert_eq!(bbox.min_lon, 0.000_000_01);
        assert_eq!(bbox.max_lat, 0.000_000_02);
        assert!(
            points(&bbox, 100, 3)
                .iter()
                .all(|&(lat, lon)| lat > 0.0 && lon > 0.0)
        );
        pbfhogg_test_scratch::refused(
            super::parse_sampling_bbox("1,1,0,2"),
            "must be less than max_lon",
        );
        pbfhogg_test_scratch::refused(
            super::parse_sampling_bbox("0,0,1"),
            "bbox needs four comma-separated values",
        );
        pbfhogg_test_scratch::refused(
            super::parse_sampling_bbox("0,0,1,91"),
            "must be within -90 and 90 degrees",
        );
    }

    #[test]
    fn points_stay_inside_world_and_tight_bboxes() {
        let boxes = [
            "-180,-90,180,90",
            "179.9999999,89.9999999,180,90",
            "-180,-90,-179.9999999,-89.9999999",
            "5.9,47.3,15.0,55.0",
            "0.00000001,0.00000001,0.00000002,0.00000002",
        ];
        for b in boxes {
            let bbox = super::parse_sampling_bbox(b).unwrap();
            for (lat, lon) in points(&bbox, 10_000, 7) {
                assert!(
                    (bbox.min_lat..=bbox.max_lat).contains(&lat),
                    "{b}: lat {lat}"
                );
                assert!(
                    (bbox.min_lon..=bbox.max_lon).contains(&lon),
                    "{b}: lon {lon}"
                );
            }
        }
    }
}
