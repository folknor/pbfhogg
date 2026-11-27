use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use crate::error::Error;
use crate::harness::Harness;
use crate::opl_compare::{self, PlainPbf, Policy, Stream};
use crate::pbf_frames::Origin;
use crate::reference_cache::{Key, Summary};
use crate::verify_reencode::KINDS;

const OBSERVATION: &str = "cat-kind-views 1";

#[derive(serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub(super) struct Observation {
    pub(super) kinds: [Kind; 3],
}

#[derive(serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub(super) struct Kind {
    preserved: bool,
    pub(super) summary: Summary,
    pub(super) without_locations: Option<Summary>,
}

impl Kind {
    pub(super) fn can_project(&self) -> bool {
        self.preserved && self.summary.ordered
    }
}

/// Observe each kind over the same base decoding. A stored observation
/// can decide equality with an output's stored reading. If it cannot,
/// prepare all views together on the first arm that needs to merge.
pub(super) struct Views {
    input: PathBuf,
    outdir: PathBuf,
    pub(super) policy: Policy,
    pub(super) key: Option<Key>,
    pub(super) observed: Observation,
    written: bool,
}

impl Views {
    pub(super) fn prepare(
        h: &mut Harness,
        input: &Path,
        outdir: &Path,
        policy: Policy,
    ) -> Result<Self, Error> {
        let stored = opl_compare::planned_keys(h, &[input], policy)?.and_then(|keys| {
            let key = keys.into_iter().next()?;
            h.summary_of::<Observation>(&key.derived(OBSERVATION))
                .map(|obs| (key, obs))
        });
        let (key, observed, written) = match stored {
            Some((key, observed)) => {
                h.run.revalidate()?;
                h.checkpoint()?;
                (Some(key), observed, false)
            }
            None => {
                let (key, observed) = prepare_views(h, input, outdir, policy)?;
                (key, observed, true)
            }
        };
        Ok(Self {
            input: input.to_path_buf(),
            outdir: outdir.to_path_buf(),
            policy,
            key,
            observed,
            written,
        })
    }

    pub(super) fn path(
        &mut self,
        h: &mut Harness,
        kind: usize,
        no_locations: bool,
    ) -> Result<PathBuf, Error> {
        if !self.written {
            let (_, observed) = prepare_views(h, &self.input, &self.outdir, self.policy)?;
            if observed != self.observed {
                return Err(Error::Environment(format!(
                    "{}: the decoded cat views differ from their stored reading",
                    self.input.display()
                )));
            }
            self.written = true;
        }
        Ok(view_path(&self.outdir, kind, no_locations))
    }
}

fn prepare_views(
    h: &mut Harness,
    input: &Path,
    outdir: &Path,
    policy: Policy,
) -> Result<(Option<Key>, Observation), Error> {
    opl_compare::decoded(h, (input, Origin::Other), policy, |h, opl| {
        let (observed, source) = h.timed("opl kind views", || write_views(opl, outdir, policy))?;
        opl_compare::observe_scan(h, opl, &source)?;
        let key = opl_compare::decoding_key(h, opl);
        if let Some(key) = &key {
            h.publish_summary_of(&key.derived(OBSERVATION), &observed)?;
        }
        Ok((key, observed))
    })
}

fn view_path(dir: &Path, kind: usize, no_locations: bool) -> PathBuf {
    let name = KINDS[kind];
    let suffix = if no_locations { "-no-locations" } else { "" };
    dir.join(format!("expected-{name}{suffix}.opl"))
}

struct Sink {
    writer: BufWriter<File>,
    digest: xxhash_rust::xxh3::Xxh3,
    ordered: bool,
    preserved: bool,
    count: u64,
    last: Option<i64>,
}

impl Sink {
    fn create(path: &Path) -> Result<Self, Error> {
        Ok(Self {
            writer: BufWriter::new(File::create(path)?),
            digest: xxhash_rust::xxh3::Xxh3::new(),
            ordered: true,
            preserved: true,
            count: 0,
            last: None,
        })
    }

    fn push(
        &mut self,
        key @ (kind, id): (usize, i64),
        line: &[u8],
        preserved: bool,
    ) -> Result<(), Error> {
        if let Some(last) = self.last.replace(id) {
            self.ordered &= opl_compare::rank((kind, last)) < opl_compare::rank(key);
        }
        self.preserved &= preserved;
        self.count += 1;
        self.writer.write_all(line)?;
        self.writer.write_all(b"\n")?;
        self.digest.update(line);
        self.digest.update(b"\n");
        Ok(())
    }

    fn finish(mut self, kind: usize) -> Result<Kind, Error> {
        self.writer.flush()?;
        let mut counts = [0; 3];
        counts[kind] = self.count;
        Ok(Kind {
            preserved: self.preserved,
            summary: Summary {
                ordered: self.ordered,
                counts,
                digest: format!("{:032x}", self.digest.digest128()),
            },
            without_locations: None,
        })
    }
}

fn write_views(opl: &Path, outdir: &Path, policy: Policy) -> Result<(Observation, Summary), Error> {
    let mut stream = Stream::open(opl)?;
    let mut plain = PlainPbf::default();
    let mut sinks = [
        Sink::create(&view_path(outdir, 0, false))?,
        Sink::create(&view_path(outdir, 1, false))?,
        Sink::create(&view_path(outdir, 2, false))?,
    ];
    let mut unlocated = policy
        .locations
        .then(|| Sink::create(&view_path(outdir, 1, true)))
        .transpose()?;
    let mut projected = Vec::new();
    while let Some(key @ (kind, _)) = stream.read_record()? {
        let line = stream.line();
        let preserved = plain.preserves(key, line);
        sinks[kind].push(key, line, preserved)?;
        if kind == 1
            && let Some(sink) = &mut unlocated
        {
            without_locations(line, &mut projected)?;
            sink.push(key, &projected, preserved)?;
        }
    }
    let [nodes, ways, relations] = sinks;
    let mut way = ways.finish(1)?;
    if let Some(unlocated) = unlocated {
        way.without_locations = Some(unlocated.finish(1)?.summary);
    }
    let observed = Observation {
        kinds: [nodes.finish(0)?, way, relations.finish(2)?],
    };
    Ok((observed, stream.summary()))
}

/// OPL places the way's `N` field last, with comma-separated references.
/// With locations enabled each reference is `n<ID>x<LON>y<LAT>` (or
/// `n<ID>xy`); with them disabled it is just `n<ID>`. Other fields have
/// identical encodings, including tags whose content looks like a ref.
fn without_locations(line: &[u8], projected: &mut Vec<u8>) -> Result<(), Error> {
    let start = line
        .iter()
        .rposition(|&b| b == b' ')
        .ok_or_else(|| Error::tool("OPL way has no node list"))?
        + 1;
    let refs = line[start..]
        .strip_prefix(b"N")
        .ok_or_else(|| Error::tool("OPL way has no final N field"))?;
    projected.clear();
    projected.extend_from_slice(&line[..=start]);
    if !refs.is_empty() {
        for (i, node) in refs.split(|&b| b == b',').enumerate() {
            let x = node
                .iter()
                .position(|&b| b == b'x')
                .ok_or_else(|| Error::tool("OPL way reference has no coordinate fields"))?;
            if i != 0 {
                projected.push(b',');
            }
            projected.extend_from_slice(&node[..x]);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn one_scan_matches_the_filtered_decodes() {
        let scratch = pbfhogg_test_scratch::scratch();
        let dir = scratch.path();
        let source = dir.join("base.opl");
        std::fs::write(
            &source,
            "w-2 v1 dV Tref=n1x3y4 Nn-1x1.5y-2.25,n3xy\n\
             n-1 v1 dV T x1.5 y-2.25\n\
             r1 v2 dV T Mw-2@outer\n\
             w3 v1 dV T N\n\
             n3 v4 dV Tname=a%20b x0 y0\n",
        )
        .unwrap();
        let (observed, source) = write_views(&source, dir, Policy::FULL).unwrap();
        let filtered = [
            "n-1 v1 dV T x1.5 y-2.25\nn3 v4 dV Tname=a%20b x0 y0\n",
            "w-2 v1 dV Tref=n1x3y4 Nn-1x1.5y-2.25,n3xy\nw3 v1 dV T N\n",
            "r1 v2 dV T Mw-2@outer\n",
        ];
        assert!(!source.ordered);
        assert_eq!(source.counts, [2, 2, 1]);
        for (kind, (expected, view)) in filtered.into_iter().zip(&observed.kinds).enumerate() {
            assert_eq!(
                std::fs::read(view_path(dir, kind, false)).unwrap(),
                expected.as_bytes()
            );
            assert!(view.summary.ordered);
            assert!(view.preserved);
            assert_eq!(
                view.summary.digest,
                format!("{:032x}", xxhash_rust::xxh3::xxh3_128(expected.as_bytes()))
            );
        }
        let expected = b"w-2 v1 dV Tref=n1x3y4 Nn-1,n3\nw3 v1 dV T N\n";
        assert_eq!(std::fs::read(view_path(dir, 1, true)).unwrap(), expected);
        assert_eq!(
            observed.kinds[1].without_locations.as_ref().unwrap().digest,
            format!("{:032x}", xxhash_rust::xxh3::xxh3_128(expected))
        );
    }

    #[test]
    fn eligibility_and_order_are_per_kind() {
        let scratch = pbfhogg_test_scratch::scratch();
        let dir = scratch.path();
        let source = dir.join("base.opl");
        std::fs::write(
            &source,
            "w1 v1 dV T Nn2\nr1 v1 dD T M\nn2 v1 dV T x1 y1\nn1 v1 dV T x1 y1\n",
        )
        .unwrap();
        let (observed, _) = write_views(&source, dir, Policy::NO_LOCATIONS).unwrap();
        let [nodes, ways, relations] = observed.kinds;
        assert!(!nodes.summary.ordered);
        assert!(ways.summary.ordered);
        assert!(ways.preserved);
        assert!(ways.without_locations.is_none());
        assert!(relations.summary.ordered);
        assert!(!relations.preserved);
    }
}
