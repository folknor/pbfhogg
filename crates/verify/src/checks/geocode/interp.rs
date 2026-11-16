//! The interpolation part of the geocode model: the ranges the
//! build-geocode-index rules in docs/cli/commands.md make of the
//! interpolation ways in the osmium decoding, and the number a query on one
//! is answered.
//!
//! A way with `addr:interpolation` and no `addr:housenumber` is an
//! interpolation way. Its mode is `all` or `1` (step 1), `even` or `odd`
//! (step 2 with that parity) or a digit `2` to `9` (that step from the lower
//! end, no parity); any other value refuses it, as does a node osmium could
//! not locate (pbfhogg cannot see that node either). A way of fewer than two
//! nodes makes no range.
//!
//! It numbers along its own non-empty `addr:street`, else its own non-empty
//! `addr:place`, else the one street the address points at its first and
//! last node share, else the one place their place addresses (those naming
//! no street) share; anything else refuses it. "At a node" is coordinate
//! equality with the node, building centroids included. At each node the
//! addresses on that street or place either are absent or carry one
//! house-number string; disagreeing strings refuse the way; an all-digit
//! string is an anchor when it is a positive u32 and refuses the way when it
//! is `0` or larger; any other string is passed over at an interior node
//! and refuses the way at an end. Consecutive anchors bound a section: equal
//! numbers, or numbers of the wrong parity on an `even` or `odd` way, refuse
//! the whole way, and a section with no number strictly between its anchors
//! is dropped. A refused way never answers.
//!
//! A query's interpolation candidate is each straight segment of each
//! section, measured as the street model measures. Its position along the
//! section is the length from the section's first node to the nearest point
//! over the section's total length, each segment's length taken in the flat
//! metric scaled by the cosine of its first node's latitude, as the
//! contract states. pbfhogg truncates the nearest point to decimicrodegrees
//! before measuring, so every number the rule gives over the fractions
//! within that truncation's reach of the model's position is accepted; a
//! query far from a rounding threshold has one answer.

use std::collections::HashMap;

use super::{Address, cell, rad};

/// How a way steps between its anchors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Mode {
    All,
    Even,
    Odd,
    Every(u32),
}

impl Mode {
    pub(super) fn from_tag(value: &str) -> Option<Self> {
        match value {
            "all" | "1" => Some(Self::All),
            "even" => Some(Self::Even),
            "odd" => Some(Self::Odd),
            _ => match value.as_bytes() {
                &[d @ b'2'..=b'9'] => Some(Self::Every(u32::from(d - b'0'))),
                _ => None,
            },
        }
    }

    fn step(self) -> u32 {
        match self {
            Self::All => 1,
            Self::Even | Self::Odd => 2,
            Self::Every(n) => n,
        }
    }

    /// Whether `a` then `b` may be consecutive anchors of one way.
    fn agree(self, a: u32, b: u32) -> bool {
        a != b
            && match self {
                Self::Even => a % 2 == 0 && b % 2 == 0,
                Self::Odd => a % 2 == 1 && b % 2 == 1,
                Self::All | Self::Every(_) => true,
            }
    }

    /// Whether some number lies strictly between `a` and `b` on the step.
    fn has_interior(self, a: u32, b: u32) -> bool {
        a.abs_diff(b) > self.step()
    }

    /// The number at fraction `f` from `start` to `end`: of `lo, lo + step,
    /// ...` not past `hi`, and `hi`, the nearest to the position measured
    /// from the lower end, a tie to the lower number.
    pub(super) fn number_at(self, start: u32, end: u32, f: f64) -> u32 {
        let f = f.clamp(0.0, 1.0);
        let (lo, hi, u) = if start <= end {
            (start, end, f)
        } else {
            (end, start, 1.0 - f)
        };
        let at = u * f64::from(hi - lo);
        let step = self.step();
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let below = ((at / f64::from(step)).floor() as u32).min((hi - lo) / step);
        // In u64: the grid number after `below` can pass u32::MAX.
        let (lo64, hi64) = (u64::from(lo), u64::from(hi));
        let mut best = lo64 + u64::from(below) * u64::from(step);
        for candidate in [best + u64::from(step), hi64] {
            if candidate > hi64 || candidate <= best {
                continue;
            }
            #[allow(clippy::cast_precision_loss)]
            let (d_best, d_candidate) = (
                (at - (best - lo64) as f64).abs(),
                ((candidate - lo64) as f64 - at).abs(),
            );
            if d_candidate < d_best {
                best = candidate;
            }
        }
        #[allow(clippy::cast_possible_truncation)]
        let best = best as u32;
        best
    }
}

/// An interpolation way as osmium decodes it.
pub(super) struct RawWay {
    pub(super) mode: Option<Mode>,
    pub(super) street: Option<String>,
    pub(super) place: Option<String>,
    pub(super) nodes: Vec<Option<(i32, i32)>>,
}

/// One answering range.
pub(super) struct Section {
    pub(super) street: Option<String>,
    pub(super) place: Option<String>,
    mode: Mode,
    start: u32,
    end: u32,
    pub(super) nodes: Vec<(i32, i32)>,
    /// Length from the first node to node `i`, radians.
    along: Vec<f64>,
}

/// The parent a way numbers along.
#[derive(Clone, PartialEq, Eq)]
enum Parent {
    Street(String),
    Place(String),
}

impl Parent {
    fn holds(&self, a: &Address) -> bool {
        match self {
            Self::Street(s) => a.street.as_deref() == Some(s),
            Self::Place(p) => a.street.is_none() && a.place.as_deref() == Some(p),
        }
    }
}

/// The length of a segment in the contract's metric, radians.
pub(super) fn segment_length(a: (i32, i32), b: (i32, i32)) -> f64 {
    let dlat = rad(i64::from(b.0)) - rad(i64::from(a.0));
    let dlon = (rad(i64::from(b.1)) - rad(i64::from(a.1))) * rad(i64::from(a.0)).cos();
    (dlat * dlat + dlon * dlon).sqrt()
}

/// What the ways resolve to: their sections, and how many were refused.
#[derive(Default)]
pub(super) struct Resolved {
    pub(super) sections: Vec<Section>,
    pub(super) resolved_ways: usize,
    pub(super) refused_ways: usize,
}

/// Resolve every way against the address points.
pub(super) fn resolve(ways: &[RawWay], addresses: &[Address]) -> Resolved {
    let mut at: HashMap<(i32, i32), Vec<usize>> = HashMap::new();
    for (i, a) in addresses.iter().enumerate() {
        at.entry((a.lat, a.lon)).or_default().push(i);
    }
    let mut out = Resolved::default();
    for way in ways {
        if way.nodes.len() < 2 {
            continue;
        }
        match resolve_way(way, addresses, &at) {
            Some(sections) => {
                out.resolved_ways += 1;
                out.sections.extend(sections);
            }
            None => out.refused_ways += 1,
        }
    }
    out
}

fn resolve_way(
    way: &RawWay,
    addresses: &[Address],
    at: &HashMap<(i32, i32), Vec<usize>>,
) -> Option<Vec<Section>> {
    let mode = way.mode?;
    let nodes: Vec<(i32, i32)> = way.nodes.iter().copied().collect::<Option<_>>()?;
    let here = |node: (i32, i32)| at.get(&node).into_iter().flatten().map(|&i| &addresses[i]);
    let parent = match (&way.street, &way.place) {
        (Some(s), _) => Parent::Street(s.clone()),
        (None, Some(p)) => Parent::Place(p.clone()),
        (None, None) => {
            let (first, last) = (nodes[0], nodes[nodes.len() - 1]);
            let shared = |name: fn(&Address) -> Option<&String>| {
                let at_last: Vec<&String> = here(last).filter_map(name).collect();
                let mut both: Vec<String> = here(first)
                    .filter_map(name)
                    .filter(|n| at_last.contains(n))
                    .cloned()
                    .collect();
                both.sort();
                both.dedup();
                both
            };
            let streets = shared(|a| a.street.as_ref());
            if streets.is_empty() {
                let places = shared(|a| {
                    if a.street.is_none() {
                        a.place.as_ref()
                    } else {
                        None
                    }
                });
                match places.as_slice() {
                    [p] => Parent::Place(p.clone()),
                    _ => return None,
                }
            } else {
                match streets.as_slice() {
                    [s] => Parent::Street(s.clone()),
                    _ => return None,
                }
            }
        }
    };
    let last = nodes.len() - 1;
    let mut sections = Vec::new();
    let mut previous: Option<(usize, u32)> = None;
    for (i, &node) in nodes.iter().enumerate() {
        let mut numbers = here(node)
            .filter(|a| parent.holds(a))
            .map(|a| &a.house_number);
        let number = match numbers.next() {
            None => None,
            Some(first) => {
                if numbers.any(|n| n != first) {
                    return None;
                }
                Some(first)
            }
        };
        let anchor = match number {
            None => None,
            Some(text) if !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) => {
                match text.parse::<u32>() {
                    Ok(n) if n > 0 => Some(n),
                    _ => return None,
                }
            }
            Some(_) => None,
        };
        let Some(n) = anchor else {
            if i == 0 || i == last {
                return None;
            }
            continue;
        };
        if let Some((from, start)) = previous {
            if !mode.agree(start, n) {
                return None;
            }
            if mode.has_interior(start, n) {
                sections.push(Section::new(&parent, mode, start, n, &nodes[from..=i]));
            }
        }
        previous = Some((i, n));
    }
    Some(sections)
}

/// What a candidate segment answers: its parent and the numbers its
/// position allows.
#[derive(Debug, Clone)]
pub(super) struct InterpAnswer {
    pub(super) street: Option<String>,
    pub(super) place: Option<String>,
    /// The numbers allowed are those of the range's grid (`lo`, `lo +
    /// step`, ... and `hi`) within `least..=most`, every one the rule gives
    /// within the reach of the truncated nearest point; kept as bounds, not
    /// listed, since a range can span most of u32.
    least: u32,
    most: u32,
    lo: u32,
    hi: u32,
    step: u32,
}

/// How far the truncated nearest point can move along the section,
/// radians: under 1e-7 degrees on each axis.
const TRUNCATION_REACH_DEG: f64 = 3e-7;

/// Relative float slack of the fraction itself.
const FRACTION_SLACK: f64 = 1e-12;

impl Section {
    fn new(parent: &Parent, mode: Mode, start: u32, end: u32, span: &[(i32, i32)]) -> Self {
        let mut along = vec![0.0];
        for pair in span.windows(2) {
            along.push(along[along.len() - 1] + segment_length(pair[0], pair[1]));
        }
        let (street, place) = match parent {
            Parent::Street(s) => (Some(s.clone()), None),
            Parent::Place(p) => (None, Some(p.clone())),
        };
        Self {
            street,
            place,
            mode,
            start,
            end,
            nodes: span.to_vec(),
            along,
        }
    }

    fn total(&self) -> f64 {
        self.along[self.along.len() - 1]
    }

    /// Whether this section can answer (pbfhogg refuses a range of no length).
    pub(super) fn measurable(&self) -> bool {
        self.total() > 0.0
    }

    /// The answer of segment `index` at parameter `t` along it.
    pub(super) fn answer(&self, index: usize, t: f64) -> InterpAnswer {
        let (a, b) = (self.nodes[index], self.nodes[index + 1]);
        let point = |p: i32, q: i32| f64::from(p) + t * (f64::from(q) - f64::from(p));
        let (lat, lon) = (point(a.0, b.0), point(a.1, b.1));
        let dlat = (lat - f64::from(a.0)) * 1e-7_f64.to_radians();
        let dlon = (lon - f64::from(a.1)) * 1e-7_f64.to_radians() * rad(i64::from(a.0)).cos();
        let total = self.total();
        let f = (self.along[index] + (dlat * dlat + dlon * dlon).sqrt()) / total;
        let reach = TRUNCATION_REACH_DEG.to_radians() / total + FRACTION_SLACK;
        let (x, y) = (
            self.mode.number_at(self.start, self.end, f - reach),
            self.mode.number_at(self.start, self.end, f + reach),
        );
        InterpAnswer {
            street: self.street.clone(),
            place: self.place.clone(),
            least: x.min(y),
            most: x.max(y),
            lo: self.start.min(self.end),
            hi: self.start.max(self.end),
            step: self.mode.step(),
        }
    }
}

impl InterpAnswer {
    /// Whether `n` is one of the numbers allowed: within `least..=most` and
    /// on the range's grid (or its upper end).
    pub(super) fn allows(&self, n: u32) -> bool {
        (self.least..=self.most).contains(&n)
            && (n == self.hi || (n >= self.lo && (n - self.lo).is_multiple_of(self.step)))
    }
}

/// A candidate segment for the grid.
#[derive(Clone, Copy)]
pub(super) struct InterpSegment {
    pub(super) section: u32,
    pub(super) index: u16,
}

/// The grid of every measurable section's segments, as the street model
/// lists streets.
pub(super) type Cells = HashMap<(i32, i32), Vec<u32>>;

pub(super) fn grid(sections: &[Section]) -> Result<(Vec<InterpSegment>, Cells), String> {
    let mut segments = Vec::new();
    let mut cells: HashMap<(i32, i32), Vec<u32>> = HashMap::new();
    for (s, section) in sections.iter().enumerate() {
        if !section.measurable() {
            continue;
        }
        for (i, pair) in section.nodes.windows(2).enumerate() {
            let (a, b) = (pair[0], pair[1]);
            let n = u32::try_from(segments.len()).map_err(|_| "too many segments")?;
            let (lo, hi) = (
                cell(a.0.min(b.0), a.1.min(b.1)),
                cell(a.0.max(b.0), a.1.max(b.1)),
            );
            for row in lo.0..=hi.0 {
                for col in lo.1..=hi.1 {
                    cells.entry((row, col)).or_default().push(n);
                }
            }
            segments.push(InterpSegment {
                section: u32::try_from(s).map_err(|_| "too many sections")?,
                index: u16::try_from(i).map_err(|_| "interpolation way too long")?,
            });
        }
    }
    Ok((segments, cells))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn the_documented_steps_answer_on_their_grid() {
        // `4` from 10 to 21 is 10, 14, 18 and 21.
        let m = Mode::Every(4);
        let at = |f: f64| m.number_at(10, 21, f);
        assert_eq!(at(0.0), 10);
        assert_eq!(at(1.0), 21);
        assert_eq!(at(4.0 / 11.0), 14);
        assert_eq!(at(9.4 / 11.0), 18);
        assert_eq!(at(9.6 / 11.0), 21);
        // The tie between 10 and 14 goes to the lower number.
        assert_eq!(at(2.0 / 11.0), 10);
        // Measured from the lower end whichever way the way runs.
        assert_eq!(m.number_at(21, 10, 1.0 - 4.0 / 11.0), 14);
        assert_eq!(Mode::Even.number_at(10, 20, 0.5), 14);
        assert_eq!(Mode::from_tag("0"), None);
        assert_eq!(Mode::from_tag("alphabetic"), None);
        assert_eq!(Mode::from_tag("1"), Some(Mode::All));
    }

    fn address(lat: i32, number: &str, street: Option<&str>, place: Option<&str>) -> Address {
        Address {
            lat,
            lon: 0,
            house_number: number.into(),
            street: street.map(Into::into),
            place: place.map(Into::into),
            postcode: None,
        }
    }

    fn way(mode: &str, lats: &[i32]) -> RawWay {
        RawWay {
            mode: Mode::from_tag(mode),
            street: None,
            place: None,
            nodes: lats.iter().map(|&l| Some((l, 0))).collect(),
        }
    }

    #[test]
    fn ways_split_at_interior_anchors_and_refuse_by_the_rules() {
        let a = [
            address(0, "10", Some("S"), None),
            address(100, "12b", Some("S"), None),
            address(200, "20", Some("S"), None),
            address(300, "40", Some("S"), None),
            address(300, "7", None, Some("P")),
        ];
        let r = resolve(&[way("even", &[0, 100, 200, 300])], &a);
        let ranges: Vec<(u32, u32)> = r.sections.iter().map(|s| (s.start, s.end)).collect();
        assert_eq!(ranges, [(10, 20), (20, 40)]);
        assert_eq!(r.sections[0].street.as_deref(), Some("S"));
        // Wrong parity refuses the whole way.
        assert_eq!(resolve(&[way("odd", &[0, 300])], &a).refused_ways, 1);
        // An end with no address on the street refuses it.
        assert_eq!(resolve(&[way("all", &[0, 50])], &a).refused_ways, 1);
        // No number strictly between: dropped, the way still resolved.
        let b = [
            address(0, "10", Some("S"), None),
            address(9, "12", Some("S"), None),
        ];
        let r = resolve(&[way("even", &[0, 9])], &b);
        assert_eq!((r.resolved_ways, r.sections.len()), (1, 0));
        // Disagreeing strings at one node refuse.
        let c = [
            address(0, "10", Some("S"), None),
            address(0, "11", Some("S"), None),
            address(9, "20", Some("S"), None),
        ];
        assert_eq!(resolve(&[way("all", &[0, 9])], &c).refused_ways, 1);
        // Place addresses decide when no street is shared.
        let d = [
            address(0, "1", None, Some("P")),
            address(9, "9", None, Some("P")),
        ];
        let r = resolve(&[way("odd", &[0, 9])], &d);
        assert_eq!(r.sections[0].place.as_deref(), Some("P"));
        // An unlocated node refuses.
        let mut w = way("all", &[0, 9]);
        w.nodes.push(None);
        assert_eq!(resolve(&[w], &d).refused_ways, 1);
    }

    #[test]
    fn a_position_near_a_threshold_allows_both_numbers() {
        let a = [
            address(0, "10", Some("S"), None),
            address(1_000, "20", Some("S"), None),
        ];
        let r = resolve(&[way("all", &[0, 1_000])], &a);
        let s = &r.sections[0];
        let allowed = |t: f64| {
            (10..=20)
                .filter(|&n| s.answer(0, t).allows(n))
                .collect::<Vec<_>>()
        };
        assert_eq!(allowed(0.31), [13]);
        assert_eq!(allowed(0.35), [13, 14]);
        // The far end of a range reaching u32::MAX.
        assert_eq!(Mode::All.number_at(u32::MAX - 2, u32::MAX, 1.0), u32::MAX);
    }
}
