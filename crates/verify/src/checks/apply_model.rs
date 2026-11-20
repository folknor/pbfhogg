//! The apply-changes check's own model of what each tool writes for an
//! identity the input OSC touches, built from the inputs alone.
//!
//! pbfhogg and osmium resolve a touched identity by different documented
//! rules (DEVIATIONS.md, "Change stream"), so neither tool's output is the
//! other's oracle there:
//!
//! - **pbfhogg** admits the change set only if no explicit OSC version
//!   claim falls below an earlier claim for the same object, and the
//!   highest explicit claim (deletes included) is not below the matched base
//!   element's version (a base version below 1 claims nothing; an OSC
//!   `version="0"` is a claim, an absent one is not). It then writes the
//!   object's last occurrence in file order, or nothing when that occurrence
//!   is a `<delete>`.
//! - **osmium**, plain mode (`command_apply_changes.cpp`), reverses the
//!   change list and stable-sorts it by `object_order_type_id_reverse_version`
//!   (version descending, then timestamp descending only when both are
//!   valid), so its candidate is the highest version, then the latest valid
//!   timestamp, then the last occurrence. A changes-first `set_union` lets the
//!   base record win only when it strictly beats the candidate, and the winner
//!   is written only when osmium decodes it visible.
//! - **osmium `--locations-on-ways`** keeps the candidate (`unique`) and merges
//!   with a cursor whose outcome, when the base does not strictly lose,
//!   depends on whether the same loop iteration consumed an earlier change:
//!   it then writes the base (and, for a visible candidate, the candidate
//!   too). That needs the base record before this one, which this model does
//!   not read, so osmium is not judged on such an identity.
//!
//! The records themselves are osmium's decodings, the projection every OPL
//! comparison here uses: the OSC decoded by `osmium cat`, joined occurrence
//! by occurrence with the verifier's own OSC scanner (which supplies the
//! action and the claim presence OPL cannot show), and the base records of
//! the touched identities from `osmium getid -H`. History mode keeps both
//! signs of an ID whose magnitude the OSC names; the plain mode would stop
//! at the first.
//!
//! The model refuses (as an unsupported comparison domain) an OSC whose
//! upsert carries a `visible` attribute other than `true` (osmium then
//! decodes the record invisible, pbfhogg's upsert semantics are undefined),
//! an explicit version outside `0..=i32::MAX` (pbfhogg's claim domain;
//! osmium folds `-1` and refuses lower), and one where some object's
//! occurrences at one version mix an invalid timestamp with two distinct
//! valid ones: osmium's comparator is then no strict weak order and its
//! whole sort, not only that object, has no defined result.

use std::collections::HashMap;

use crate::error::Error;
use crate::opl_compare::{parse_key, rank};
use crate::osc::{OscAction, OscChange};

/// A kind (node 0, way 1, relation 2) and signed ID.
pub(crate) type Key = (usize, i64);

/// What osmium decoded of one record: its version (absent is 0), its
/// timestamp as OPL prints it (empty when invalid) and its visibility.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Seen {
    version: i64,
    timestamp: Vec<u8>,
    visible: bool,
}

impl Seen {
    fn of(line: &[u8]) -> Result<Self, String> {
        let mut version = None;
        let mut timestamp = None;
        let mut visible = None;
        for token in line.split(|&b| b == b' ').skip(1) {
            let Some((&field, rest)) = token.split_first() else {
                continue;
            };
            match field {
                b'v' => {
                    version = std::str::from_utf8(rest)
                        .ok()
                        .and_then(|s| s.parse::<i64>().ok());
                }
                b't' => timestamp = Some(rest.to_vec()),
                b'd' => visible = Some(rest == b"V"),
                _ => {}
            }
        }
        let (Some(version), Some(timestamp), Some(visible)) = (version, timestamp, visible) else {
            return Err("an OPL record without its version, timestamp and visibility".into());
        };
        // libosmium's Timestamp is valid only when nonzero.
        let timestamp = if timestamp == b"1970-01-01T00:00:00Z" {
            Vec::new()
        } else {
            timestamp
        };
        Ok(Self {
            version,
            timestamp,
            visible,
        })
    }

    fn valid_timestamp(&self) -> bool {
        !self.timestamp.is_empty()
    }

    /// libosmium's `object_order_type_id_reverse_version` for one identity:
    /// whether `self` sorts strictly before `other`.
    fn beats(&self, other: &Self) -> bool {
        self.version > other.version
            || (self.version == other.version
                && self.valid_timestamp()
                && other.valid_timestamp()
                && self.timestamp > other.timestamp)
    }
}

/// One record as osmium decoded it, with its OPL line.
#[derive(Debug, Clone)]
struct Record {
    seen: Seen,
    line: Vec<u8>,
}

impl Record {
    fn of(line: Vec<u8>) -> Result<Self, String> {
        Ok(Self {
            seen: Seen::of(&line)?,
            line,
        })
    }
}

/// The timestamps one object's occurrences at one version carry, as far as
/// the comparator's validity needs them.
#[derive(Default)]
struct VersionGroup {
    invalid: bool,
    valid: Option<Vec<u8>>,
    distinct_valid: bool,
}

/// One touched identity.
struct Identity {
    /// The highest explicit claim so far.
    floor: Option<i64>,
    /// The first explicit claim below the floor, with that floor.
    regression: Option<(i64, i64)>,
    last_is_delete: bool,
    last: Vec<u8>,
    /// osmium's candidate among the changes.
    candidate: Record,
}

/// What osmium is expected to write for a touched identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OsmiumExpect {
    /// The model does not decide it (the `-l` cursor case).
    Undefined,
    Absent,
    Record(Vec<u8>),
}

/// The model over one OSC and the base records of its touched identities.
pub(crate) struct Model {
    identities: HashMap<Key, Identity>,
    base: HashMap<Key, Record>,
}

/// Why pbfhogg must refuse the change set, or `None`.
pub(crate) type Refusal = Option<String>;

fn kind_name(kind: usize) -> &'static str {
    ["node", "way", "relation"][kind]
}

fn domain(why: &str) -> Error {
    Error::tool(format!(
        "apply-changes input outside the verifier's comparison domain: {why}"
    ))
}

impl Model {
    /// Fold `changes` (the scanner's entries in file order) with `lines`
    /// (osmium's OPL decoding of the same OSC, one per entry).
    pub(crate) fn build(
        changes: &[OscChange],
        mut lines: impl Iterator<Item = Result<Vec<u8>, Error>>,
    ) -> Result<Self, Error> {
        let mut identities: HashMap<Key, Identity> = HashMap::new();
        let mut groups: HashMap<(Key, i64), VersionGroup> = HashMap::new();
        for (n, change) in changes.iter().enumerate() {
            let line = lines.next().transpose()?.ok_or_else(|| {
                Error::Failed(format!(
                    "osmium decoded {n} records of an OSC whose scan holds {}",
                    changes.len()
                ))
            })?;
            let key = (change.kind.index(), change.id);
            let decoded = parse_key(&line)
                .map_err(|why| Error::Failed(format!("osmium's OSC decoding: {why}")))?;
            if decoded != key {
                return Err(Error::Failed(format!(
                    "osmium's OSC decoding holds {} {} at occurrence {n} where the OSC holds {} {}",
                    kind_name(decoded.0),
                    decoded.1,
                    kind_name(key.0),
                    key.1
                )));
            }
            let entry = format!("{} {} (occurrence {n})", kind_name(key.0), key.1);
            if change.action != OscAction::Delete
                && change.meta.visible.as_deref().is_some_and(|v| v != "true")
            {
                return Err(domain(&format!(
                    "{entry} is an upsert with visible={:?}",
                    change.meta.visible.as_deref().unwrap_or_default()
                )));
            }
            let claim = change.meta.version;
            if let Some(claim) = claim
                && !(0..=i64::from(i32::MAX)).contains(&claim)
            {
                return Err(domain(&format!("{entry} claims version {claim}")));
            }
            let record = Record::of(line.clone())
                .map_err(|why| Error::Failed(format!("osmium's OSC decoding of {entry}: {why}")))?;
            let group = groups.entry((key, record.seen.version)).or_default();
            if record.seen.valid_timestamp() {
                match &group.valid {
                    Some(ts) if *ts != record.seen.timestamp => group.distinct_valid = true,
                    Some(_) => {}
                    None => group.valid = Some(record.seen.timestamp.clone()),
                }
            } else {
                group.invalid = true;
            }
            if group.invalid && group.distinct_valid {
                return Err(domain(&format!(
                    "{} {} has version {} occurrences with an invalid timestamp beside two \
                     distinct valid ones, for which osmium's sort has no defined result",
                    kind_name(key.0),
                    key.1,
                    record.seen.version
                )));
            }
            let is_delete = change.action == OscAction::Delete;
            match identities.get_mut(&key) {
                None => {
                    identities.insert(
                        key,
                        Identity {
                            floor: claim,
                            regression: None,
                            last_is_delete: is_delete,
                            last: line,
                            candidate: record,
                        },
                    );
                }
                Some(identity) => {
                    if let Some(claim) = claim {
                        match identity.floor {
                            Some(floor) if claim < floor => {
                                identity.regression.get_or_insert((claim, floor));
                            }
                            _ => identity.floor = Some(claim),
                        }
                    }
                    identity.last_is_delete = is_delete;
                    identity.last = line;
                    // A later occurrence takes over unless the candidate
                    // strictly beats it: reversed, then stable-sorted.
                    if !identity.candidate.seen.beats(&record.seen) {
                        identity.candidate = record;
                    }
                }
            }
        }
        if lines.next().transpose()?.is_some() {
            return Err(Error::Failed(format!(
                "osmium decoded more records than the {} the OSC scan holds",
                changes.len()
            )));
        }
        Ok(Self {
            identities,
            base: HashMap::new(),
        })
    }

    /// How many identities the OSC touches.
    pub(crate) fn identities(&self) -> usize {
        self.identities.len()
    }

    /// How many touched identities the base holds.
    pub(crate) fn base_records(&self) -> usize {
        self.base.len()
    }

    /// Whether the OSC touches `key`.
    pub(crate) fn touches(&self, key: Key) -> bool {
        self.identities.contains_key(&key)
    }

    /// Take the base records `osmium getid -H` found (any order, a superset
    /// by ID magnitude); only touched signed identities are kept, and a
    /// touched identity held twice is a history base, outside the domain.
    pub(crate) fn add_base(&mut self, line: Vec<u8>) -> Result<(), Error> {
        let key =
            parse_key(&line).map_err(|why| Error::Failed(format!("osmium getid output: {why}")))?;
        if !self.touches(key) {
            return Ok(());
        }
        let record =
            Record::of(line).map_err(|why| Error::Failed(format!("osmium getid output: {why}")))?;
        if self.base.insert(key, record).is_some() {
            return Err(domain(&format!(
                "the base holds {} {} more than once",
                kind_name(key.0),
                key.1
            )));
        }
        Ok(())
    }

    /// Why pbfhogg must refuse this change set against this base, or `None`:
    /// the first object in object order whose claims regress, within the
    /// changes or below its base version.
    pub(crate) fn refusal(&self) -> Refusal {
        let mut keys: Vec<&Key> = self.identities.keys().collect();
        keys.sort_by_key(|&&k| rank(k));
        for key in keys {
            let identity = &self.identities[key];
            let name = format!("{} {}", kind_name(key.0), key.1);
            if let Some((claim, floor)) = identity.regression {
                return Some(format!(
                    "{name} claims version {claim} after version {floor} in the changes"
                ));
            }
            let base = self
                .base
                .get(key)
                .map(|b| b.seen.version)
                .filter(|&v| v >= 1);
            if let (Some(floor), Some(base)) = (identity.floor, base)
                && floor < base
            {
                return Some(format!(
                    "{name}: the changes claim at most version {floor}, the base holds {base}"
                ));
            }
        }
        None
    }

    /// The touched identities in object order, each with the record pbfhogg
    /// must write (`None`: none) and what osmium is expected to write.
    pub(crate) fn expectations(
        &self,
        locations_on_ways: bool,
    ) -> Vec<(Key, Option<&[u8]>, OsmiumExpect)> {
        let mut out: Vec<_> = self
            .identities
            .iter()
            .map(|(&key, identity)| {
                let pbfhogg = (!identity.last_is_delete).then_some(identity.last.as_slice());
                let osmium =
                    osmium_expect(&identity.candidate, self.base.get(&key), locations_on_ways);
                (key, pbfhogg, osmium)
            })
            .collect();
        out.sort_by_key(|&(key, ..)| rank(key));
        out
    }
}

/// What osmium writes for an identity whose change candidate is `candidate`
/// against base record `base`.
fn osmium_expect(candidate: &Record, base: Option<&Record>, locations: bool) -> OsmiumExpect {
    let winner = match base {
        Some(base) if !candidate.seen.beats(&base.seen) && locations => {
            return OsmiumExpect::Undefined;
        }
        Some(base) if base.seen.beats(&candidate.seen) => base,
        _ => candidate,
    };
    if winner.seen.visible {
        OsmiumExpect::Record(winner.line.clone())
    } else {
        OsmiumExpect::Absent
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::osc::{OscKind, OscMeta};

    fn change(action: OscAction, id: i64, version: Option<i64>) -> OscChange {
        OscChange {
            action,
            kind: OscKind::Node,
            id,
            meta: OscMeta {
                version,
                ..OscMeta::default()
            },
        }
    }

    fn line(id: i64, v: i64, t: &str, visible: bool) -> Vec<u8> {
        format!(
            "n{id} v{v} d{} c1 t{t} i1 u T x1 y1",
            if visible { 'V' } else { 'D' }
        )
        .into_bytes()
    }

    fn model(entries: &[(OscChange, Vec<u8>)]) -> Result<Model, Error> {
        let changes: Vec<OscChange> = entries.iter().map(|(c, _)| c.clone()).collect();
        Model::build(&changes, entries.iter().map(|(_, l)| Ok(l.clone())))
    }

    const T1: &str = "2020-01-01T00:00:00Z";
    const T2: &str = "2020-01-02T00:00:00Z";

    #[test]
    fn pbfhogg_takes_the_last_occurrence_and_osmium_the_latest_timestamp() {
        use OscAction::Modify;
        let m = model(&[
            (change(Modify, 1, Some(2)), line(1, 2, T2, true)),
            (change(Modify, 1, Some(2)), line(1, 2, T1, true)),
        ])
        .unwrap();
        assert_eq!(m.refusal(), None);
        let e = m.expectations(false);
        assert_eq!(e[0].1, Some(line(1, 2, T1, true).as_slice()));
        assert_eq!(e[0].2, OsmiumExpect::Record(line(1, 2, T2, true)));
    }

    #[test]
    fn an_equal_version_base_with_a_later_timestamp_wins_only_in_osmium() {
        let mut m =
            model(&[(change(OscAction::Modify, 1, Some(3)), line(1, 3, T1, true))]).unwrap();
        m.add_base(line(1, 3, T2, true)).unwrap();
        assert_eq!(m.refusal(), None);
        let e = m.expectations(false);
        assert_eq!(e[0].1, Some(line(1, 3, T1, true).as_slice()));
        assert_eq!(e[0].2, OsmiumExpect::Record(line(1, 3, T2, true)));
        // Under -l the cursor decides: no expectation.
        assert_eq!(m.expectations(true)[0].2, OsmiumExpect::Undefined);
    }

    #[test]
    fn a_claimless_delete_against_a_versioned_base_deletes_only_in_pbfhogg() {
        let mut m = model(&[(change(OscAction::Delete, 1, None), line(1, 0, "", false))]).unwrap();
        m.add_base(line(1, 5, T1, true)).unwrap();
        assert_eq!(m.refusal(), None);
        let e = m.expectations(false);
        assert_eq!(e[0].1, None);
        assert_eq!(e[0].2, OsmiumExpect::Record(line(1, 5, T1, true)));
    }

    #[test]
    fn claims_below_the_base_or_an_earlier_claim_are_refusals() {
        use OscAction::{Delete, Modify};
        let mut m = model(&[
            (change(Modify, 1, Some(9)), line(1, 9, T1, true)),
            (change(Delete, 1, None), line(1, 0, "", false)),
        ])
        .unwrap();
        m.add_base(line(1, 10, T1, true)).unwrap();
        assert!(m.refusal().unwrap().contains("at most version 9"));

        let m = model(&[
            (change(Modify, 2, Some(5)), line(2, 5, T1, true)),
            (change(Modify, 2, Some(4)), line(2, 4, T1, true)),
        ])
        .unwrap();
        assert!(m.refusal().unwrap().contains("version 4 after version 5"));

        // v9 then v11 over base v10 is admitted; base version 0 claims
        // nothing.
        let mut m = model(&[
            (change(Modify, 3, Some(9)), line(3, 9, T1, true)),
            (change(Modify, 3, Some(11)), line(3, 11, T1, true)),
        ])
        .unwrap();
        m.add_base(line(3, 10, T1, true)).unwrap();
        assert_eq!(m.refusal(), None);
        let mut m = model(&[(change(Modify, 4, Some(0)), line(4, 0, T1, true))]).unwrap();
        m.add_base(line(4, 0, T1, true)).unwrap();
        assert_eq!(m.refusal(), None);
    }

    #[test]
    fn untouched_base_records_are_ignored_and_a_repeated_one_refused() {
        let mut m =
            model(&[(change(OscAction::Modify, 1, Some(2)), line(1, 2, T1, true))]).unwrap();
        // getid -H matches by magnitude: n-1 comes back too.
        m.add_base(line(-1, 7, T1, true)).unwrap();
        assert_eq!(m.refusal(), None);
        m.add_base(line(1, 1, T1, true)).unwrap();
        assert!(matches!(
            m.add_base(line(1, 1, T1, true)),
            Err(Error::Tool(_))
        ));
    }

    #[test]
    fn the_domain_and_the_join_are_enforced() {
        use OscAction::Modify;
        let mut hidden = change(Modify, 1, Some(1));
        hidden.meta.visible = Some("false".into());
        assert!(matches!(
            model(&[(hidden, line(1, 1, T1, false))]),
            Err(Error::Tool(_))
        ));
        assert!(matches!(
            model(&[(change(Modify, 1, Some(-1)), line(1, 0, T1, true))]),
            Err(Error::Tool(_))
        ));
        assert!(matches!(
            model(&[
                (change(Modify, 1, Some(2)), line(1, 2, T1, true)),
                (change(Modify, 1, Some(2)), line(1, 2, "", true)),
                (change(Modify, 1, Some(2)), line(1, 2, T2, true)),
            ]),
            Err(Error::Tool(_))
        ));
        assert!(matches!(
            model(&[(change(Modify, 1, Some(2)), line(2, 2, T1, true))]),
            Err(Error::Failed(_))
        ));
    }
}
