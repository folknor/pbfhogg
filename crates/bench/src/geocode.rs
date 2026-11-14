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
/// once. Reports the timed rounds' total as `elapsed_ms`, each round's
/// per-query mean, and answer tallies plus a digest of the complete
/// answers, so two runs can be checked for answering alike.
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
        tally,
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
    tally.report();
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
    tally: Tally,
    round_ns: Vec<u128>,
    elapsed_ms: u128,
}

/// The warm-up pass, which is also the answer tally, then `rounds` timed
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

    // Answers do not change between rounds, so the timed rounds keep only a
    // sink against the optimizer.
    let mut tally = Tally::new();
    for (i, &(lat, lon)) in points.iter().enumerate() {
        let r = query(lat, lon).map_err(|e| failed("warm-up", i, e))?;
        tally.add(&r);
    }
    markers.marker("GEOCODE_QUERY_WARMUP_END");
    if cancelled() {
        return Err(crate::Failure::Cancelled);
    }

    let mut sink = 0usize;
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
        tally,
        round_ns,
        elapsed_ms,
    })
}

/// `queries` (lat, lon) points uniform in `bbox`, a pure function of bbox
/// and seed (splitmix64). Each coordinate is clamped to its bbox interval,
/// since the scaling can round an ulp past the upper bound, and at a world
/// bound that point would be out of range; the clamp moves only such edge
/// points, so revisions query the same points.
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

/// What one pass answered: counts per class, and an FNV-1a digest of every
/// ReverseResult field, in query and answer order. reverse-result-v1 uses
/// length-prefixed UTF-8, option tags and float bits.
#[derive(Default)]
struct Tally {
    admin_answered: u64,
    admin_matches: u64,
    addresses: u64,
    streets: u64,
    interpolations: u64,
    digest: u64,
}

impl Tally {
    fn new() -> Self {
        Self {
            digest: 0xCBF2_9CE4_8422_2325,
            ..Self::default()
        }
    }

    fn add(&mut self, r: &ReverseResult<'_>) {
        use pbfhogg::geocode_index::reader::{
            AddressMatch, AdminMatch, InterpolationMatch, StreetMatch,
        };
        // Exhaustive patterns force additions to the answer schema to be
        // considered here rather than silently omitted from its evidence.
        let ReverseResult {
            address,
            street,
            interpolation,
            admin,
        } = r;
        self.admin_answered += u64::from(!admin.is_empty());
        self.admin_matches += admin.len() as u64;
        self.addresses += u64::from(address.is_some());
        self.streets += u64::from(street.is_some());
        self.interpolations += u64::from(interpolation.is_some());
        // Schema order, option tags, lengths and little-endian float bits make
        // every field and query boundary unambiguous. Only warm-up calls add().
        self.hash(&[0xFF]);
        self.hash(&[u8::from(address.is_some())]);
        if let Some(AddressMatch {
            lat_e7,
            lon_e7,
            house_number,
            street,
            place,
            postcode,
            distance_m,
        }) = address
        {
            self.hash(&lat_e7.to_le_bytes());
            self.hash(&lon_e7.to_le_bytes());
            self.text(house_number);
            self.optional_text(*street);
            self.optional_text(*place);
            self.optional_text(*postcode);
            self.hash(&distance_m.to_bits().to_le_bytes());
        }
        self.hash(&[u8::from(street.is_some())]);
        if let Some(StreetMatch {
            name,
            snap_lat_e7,
            snap_lon_e7,
            distance_m,
        }) = street
        {
            self.text(name);
            self.hash(&snap_lat_e7.to_le_bytes());
            self.hash(&snap_lon_e7.to_le_bytes());
            self.hash(&distance_m.to_bits().to_le_bytes());
        }
        self.hash(&[u8::from(interpolation.is_some())]);
        if let Some(InterpolationMatch {
            street,
            place,
            house_number,
            distance_m,
        }) = interpolation
        {
            self.optional_text(*street);
            self.optional_text(*place);
            self.hash(&house_number.to_le_bytes());
            self.hash(&distance_m.to_bits().to_le_bytes());
        }
        self.hash(&(admin.len() as u64).to_le_bytes());
        for AdminMatch {
            admin_level,
            name,
            country_code,
            area,
        } in admin
        {
            self.hash(&[*admin_level]);
            self.text(name);
            self.optional_text(*country_code);
            self.hash(&area.to_bits().to_le_bytes());
        }
    }

    fn text(&mut self, value: &str) {
        self.hash(&(value.len() as u64).to_le_bytes());
        self.hash(value.as_bytes());
    }

    fn optional_text(&mut self, value: Option<&str>) {
        self.hash(&[u8::from(value.is_some())]);
        if let Some(value) = value {
            self.text(value);
        }
    }

    fn hash(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.digest ^= u64::from(b);
            self.digest = self.digest.wrapping_mul(0x0100_0000_01B3);
        }
    }

    fn report(&self) {
        eprintln!("admin_answered={}", self.admin_answered);
        eprintln!("admin_matches={}", self.admin_matches);
        eprintln!("addresses={}", self.addresses);
        eprintln!("streets={}", self.streets);
        eprintln!("interpolations={}", self.interpolations);
        eprintln!("result_format=reverse-result-v1");
        eprintln!("answer_digest={:016x}", self.digest);
    }
}

/// The instrument refuses a query error in either phase rather than timing
/// less work, its points stay inside the bbox, and the digest separates
/// every answer that differs.
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::{Tally, passes, points};
    use pbfhogg::geocode_index::reader::{QueryError, ReverseResult};

    fn empty() -> ReverseResult<'static> {
        ReverseResult {
            address: None,
            street: None,
            interpolation: None,
            admin: Vec::new(),
        }
    }

    fn complete() -> ReverseResult<'static> {
        use pbfhogg::geocode_index::reader::{
            AddressMatch, AdminMatch, InterpolationMatch, StreetMatch,
        };
        ReverseResult {
            address: Some(AddressMatch {
                lat_e7: 1,
                lon_e7: 2,
                house_number: "3",
                street: Some("road"),
                place: Some("place"),
                postcode: Some("zip"),
                distance_m: 4.0,
            }),
            street: Some(StreetMatch {
                name: "road",
                snap_lat_e7: 5,
                snap_lon_e7: 6,
                distance_m: 7.0,
            }),
            interpolation: Some(InterpolationMatch {
                street: Some("road"),
                place: None,
                house_number: 8,
                distance_m: 9.0,
            }),
            admin: vec![AdminMatch {
                admin_level: 2,
                name: "country",
                country_code: Some("AB"),
                area: 10.0,
            }],
        }
    }

    fn digest(results: &[ReverseResult<'_>]) -> u64 {
        let mut tally = Tally::new();
        for result in results {
            tally.add(result);
        }
        tally.digest
    }

    #[test]
    fn full_answer_digest_covers_equal_count_field_changes() {
        let baseline = digest(&[complete()]);
        macro_rules! changed {
            ($field:ident, $member:ident, $value:expr) => {{
                let mut result = complete();
                result.$field.as_mut().expect("present").$member = $value;
                assert_ne!(
                    digest(&[result]),
                    baseline,
                    "{}",
                    stringify!($field.$member)
                );
            }};
        }
        changed!(address, lat_e7, 11);
        changed!(address, lon_e7, 12);
        changed!(address, house_number, "33");
        changed!(address, street, Some("other"));
        changed!(address, place, Some("other"));
        changed!(address, postcode, Some("other"));
        changed!(address, distance_m, -0.0);
        changed!(street, name, "other");
        changed!(street, snap_lat_e7, 15);
        changed!(street, snap_lon_e7, 16);
        changed!(street, distance_m, -0.0);
        changed!(interpolation, street, Some("other"));
        changed!(interpolation, place, Some("other"));
        changed!(interpolation, house_number, 18);
        changed!(interpolation, distance_m, -0.0);
        for field in 0..4 {
            let mut result = complete();
            match field {
                0 => result.admin[0].admin_level = 3,
                1 => result.admin[0].name = "other",
                2 => result.admin[0].country_code = Some("CD"),
                _ => result.admin[0].area = -0.0,
            }
            assert_ne!(digest(&[result]), baseline);
        }
        changed!(address, postcode, None);
        changed!(address, postcode, Some(""));
        // Float bit identity, rather than numeric equality.
        let mut positive = complete();
        positive.street.as_mut().expect("street").distance_m = 0.0;
        let mut negative = complete();
        negative.street.as_mut().expect("street").distance_m = -0.0;
        assert_ne!(digest(&[positive]), digest(&[negative]));
    }

    #[test]
    fn full_answer_digest_preserves_options_lengths_and_boundaries() {
        let mut absent = complete();
        absent.address.as_mut().expect("address").postcode = None;
        let mut empty_text = complete();
        empty_text.address.as_mut().expect("address").postcode = Some("");
        assert_ne!(digest(&[absent]), digest(&[empty_text]));
        let mut left = complete();
        left.address.as_mut().expect("address").street = Some("a\0b");
        left.address.as_mut().expect("address").place = Some("c");
        let mut right = complete();
        right.address.as_mut().expect("address").street = Some("a");
        right.address.as_mut().expect("address").place = Some("b\0c");
        assert_ne!(digest(&[left]), digest(&[right]));
        assert_ne!(
            digest(&[empty(), complete()]),
            digest(&[complete(), empty()])
        );
        assert_ne!(digest(&[empty()]), digest(&[empty(), empty()]));
        let mut combined = complete();
        combined.admin.push(complete().admin.remove(0));
        let mut no_admin = complete();
        no_admin.admin.clear();
        assert_ne!(
            digest(&[combined, no_admin]),
            digest(&[complete(), complete()])
        );
        for field in 0..3 {
            let mut result = complete();
            match field {
                0 => result.address = None,
                1 => result.street = None,
                _ => result.interpolation = None,
            }
            assert_ne!(digest(&[result]), digest(&[complete()]));
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

    #[test]
    fn points_stay_inside_world_and_tight_bboxes() {
        let boxes = [
            "-180,-90,180,90",
            "179.9999999,89.9999999,180,90",
            "-180,-90,-179.9999999,-89.9999999",
            "5.9,47.3,15.0,55.0",
        ];
        for b in boxes {
            let bbox = pbfhogg::extract::parse_bbox(b).unwrap();
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
