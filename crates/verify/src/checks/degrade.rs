//! Verify: degrade - pbfhogg `degrade`'s decode path, `--strip-locations`,
//! `--drop-ids N:SEED`, `--unsort` and `--unsort-intra`, each at the default
//! cap, against osmium's reading of the input (the shared rules and readers
//! are [`crate::verify_reencode`]).
//!
//! The reference is the input without way coordinates, since every
//! re-encode drops them, partitioned by kind when osmium does not read it in
//! order. Every case must claim no `LocationsOnWays`, claim
//! `Sort.Type_then_ID` as the input does except under an unsort mode
//! (never), carry indexdata on every blob, true by the verifier's own blob
//! decode and by `check --index`, and hold no blob over the cap or empty,
//! kinds in order.
//!
//! - `--strip-locations`: the element sequence unchanged; on an indexed input
//!   each input blob re-encoded alone ([`split`]).
//! - `--drop-ids`: exactly N objects removed, nothing added or changed, each
//!   removed object's line a `-` alone; the layout predicted with each input
//!   blob's survivors. Run twice with one seed (the outputs identical) and
//!   once with another (held to the same checks). Which N objects go is not
//!   checked against pbfhogg's selection, which is not documented, and
//!   whether another seed chose another set is coverage, never a verdict.
//! - `--unsort`: the comparison finds exactly one object moved per kind with
//!   more objects than the cap, its two lines identical; each kind packed
//!   continuously ([`continuous`]); exactly that kind's first two blobs
//!   overlap. The moved object `a` sits first in the kind's second blob,
//!   between the last two IDs `p < a < q` of its first blob of `cap`
//!   elements (decoded by the verifier): with the rest of the sequence unchanged
//!   and ascending, that places the input's objects `cap` and `cap + 1`
//!   swapped, and nothing else.
//! - `--unsort-intra`: one object moved per kind with two or more, the
//!   kind's lowest ID by osmium fileinfo, decoded second in the kind's first
//!   blob: the first two objects swapped. No blob ranges overlap.
//!
//! On a `LocationsOnWays` input every case strips way coordinates (the
//! decode path drops them whatever the flags), and the check first holds
//! osmium's reading of the input with coordinates against the reference
//! without them, to establish that some way carried one. [`run`] takes any
//! input and names an input without coordinates as a gap;
//! [`run_locations`] (the suite's `degrade -l` on the `altw-keep-untagged`
//! base, or `--locations-on-ways`) fails on one instead.
//!
//! `--drop-ids` and the unsort modes need an input osmium reads in strict
//! order and holds no negative ID (their predictions read claimed ranges
//! numerically, which canonical order with negatives need not ascend in);
//! on other input they do not run and the PASS line says so.
//! `--force` is passed only on an unindexed input.

use std::path::{Path, PathBuf};

use crate::argv::PbfArgv;
use crate::error::Error;
use crate::harness::{Harness, Verdict};
use crate::verify_reencode::{
    self as reencode, Compared, DEFAULT_CAP, Input, KINDS, LayoutCheck, Listing, Ran, References,
    continuous, split,
};

/// The drop seeds: the first runs twice, the second once.
const SEEDS: [u64; 2] = [117, 7117];
/// The most objects `--drop-ids` removes.
const MAX_DROP: u64 = 10_000;

/// What a run of the arm accumulates across its cases.
struct Run<'a> {
    harness: &'a mut Harness,
    outdir: PathBuf,
    input: &'a Input,
    direct_io: bool,
    gaps: Vec<String>,
    verified: u32,
}

impl Run<'_> {
    /// Run one degrade; `None` when pbfhogg refused it as too large.
    fn degrade(&mut self, stem: &str, flags: &[&str]) -> Result<Option<PathBuf>, Error> {
        let out = self.outdir.join(format!("degrade-{stem}.osm.pbf"));
        let mut argv = PbfArgv::new("degrade");
        for flag in flags {
            argv = argv.arg(*flag);
        }
        let argv = argv
            .arg_if(!self.input.indexed, "--force")
            .arg_if(self.direct_io, "--direct-io")
            .input(&self.input.path)
            .output(&out);
        let label = format!("pbfhogg degrade {}", flags.join(" "));
        match reencode::reencode(self.harness, argv, &label)? {
            Ran::Written => {
                self.verified += 1;
                Ok(Some(out))
            }
            Ran::TooLarge => {
                self.gaps.push(format!(
                    "pbfhogg refused degrade {} as too large for the format; its \
                     representability and correctness were not established",
                    flags.join(" ")
                ));
                Ok(None)
            }
        }
    }

    /// The checks every output gets: header, indexdata, sizes and kind order.
    fn common(
        &mut self,
        label: &str,
        out: &Path,
        sorted: bool,
    ) -> Result<reencode::OutputBlobs, Error> {
        reencode::judge_header(self.harness, label, out, sorted, false)?;
        reencode::judge_index(self.harness, &self.outdir, label, out)?;
        let blobs = reencode::output_blobs(out, DEFAULT_CAP)?;
        reencode::record_blobs(self.harness, label, &blobs);
        Ok(blobs)
    }

    fn fail(&mut self, label: &str, why: &str) {
        self.harness.msg(&format!("  {label}: {why} FAIL"));
        self.harness.fail(format!("{label}: {why}"));
    }
}

/// Run degrade cross-validation (module doc) on any input; one without
/// inline way coordinates is a PASS-line gap.
pub fn run(harness: &mut Harness, pbf: &Path, direct_io: bool) -> Result<Option<String>, Error> {
    run_on(harness, pbf, direct_io, false)
}

/// Run degrade cross-validation on an input that must carry inline way
/// coordinates: one that declares no `LocationsOnWays`, or whose ways carry
/// none, fails instead of passing with a gap.
pub fn run_locations(
    harness: &mut Harness,
    pbf: &Path,
    direct_io: bool,
) -> Result<Option<String>, Error> {
    run_on(harness, pbf, direct_io, true)
}

fn run_on(
    harness: &mut Harness,
    pbf: &Path,
    direct_io: bool,
    require_locations: bool,
) -> Result<Option<String>, Error> {
    let outdir = harness.subdir("degrade")?;
    harness.msg("--- degrade: reading the input ---");
    let input = reencode::read_input(harness, &outdir, pbf)?;
    if require_locations && !input.locations {
        return Err(Error::Failed(
            "the input declares no LocationsOnWays, so coordinate stripping cannot be verified \
             on it"
                .to_owned(),
        ));
    }
    let mut refs = References::new(&outdir, &input);
    let reference = refs.get(harness, false)?;
    let coordinates = if input.locations {
        observed_coordinates(harness, &outdir, &mut refs, &reference)?
    } else {
        false
    };
    if input.locations && !coordinates && require_locations {
        return Err(Error::Failed(
            "the input declares LocationsOnWays but osmium finds no way whose coordinates the \
             reference lost, so coordinate stripping cannot be verified on it"
                .to_owned(),
        ));
    }
    let mut run = Run {
        harness,
        outdir: outdir.clone(),
        input: &input,
        direct_io,
        gaps: Vec::new(),
        verified: 0,
    };

    strip_locations(&mut run, &reference)?;
    // The drop and unsort predictions walk each kind's claimed ID ranges as
    // ascending and disjoint in file order (`predict_drop`, the overlap
    // tally, `place_cross`, `place_intra`'s lowest ID as the first).
    // Canonical order can break that once a kind holds a negative ID (`0,
    // -1, -2` then `-3, -4` descend numerically), so any negative element ID
    // skips the cases: a conservative coverage limit of the verifier, not of
    // pbfhogg, which degrades such input like any other.
    let negative = input.info.negative_id();
    if input.info.strictly_ordered() && input.total() >= 4 && negative.is_none() {
        drop_ids(&mut run, &reference)?;
        unsort(&mut run, &reference, false)?;
        unsort(&mut run, &reference, true)?;
    } else if let Some((kind, id)) = negative {
        run.gaps.push(format!(
            "the input holds {} ID {id}, and the verifier's layout predictions read claimed \
             ranges numerically: --drop-ids, --unsort and --unsort-intra did not run",
            KINDS[kind]
        ));
    } else {
        run.gaps.push(if input.info.strictly_ordered() {
            "the input holds fewer than four objects: --drop-ids, --unsort and --unsort-intra \
             did not run"
                .to_owned()
        } else {
            "osmium reads the input out of strict order: --drop-ids, --unsort and --unsort-intra \
             did not run"
                .to_owned()
        });
    }

    if run.verified == 0 {
        return Err(Error::Failed(
            "pbfhogg refused every degrade as too large for the format: nothing was re-encoded, \
             so nothing was verified"
                .to_owned(),
        ));
    }
    if !input.indexed {
        run.gaps.push(
            "the input carries no indexdata, so the layout of --strip-locations and --drop-ids \
             was not predicted"
                .to_owned(),
        );
    }
    if !input.locations {
        run.gaps.push(
            "the input declares no LocationsOnWays, so no case had a way coordinate to strip"
                .to_owned(),
        );
    } else if !coordinates {
        run.gaps.push(
            "the input declares LocationsOnWays but no way carries a coordinate, so no case had \
             one to strip"
                .to_owned(),
        );
    }
    if !input.standard {
        run.gaps.push(
            "osmium does not read the input in kind and ID order: elements compared with \
             osmium's stable partition of it by kind"
                .to_owned(),
        );
    }
    let mut claim = vec![
        "element sequence with metadata, record by record against osmium's OPL decodings, header claims, indexdata against payloads (the verifier's own blob decode for \
         kind, count and ID range, check --index for the rest), cap and kind order, \
         the documented layout where it is predicted, and each unsort swap's position; which \
         objects --drop-ids removes is not compared with pbfhogg's selection"
            .to_owned(),
    ];
    claim.extend(run.gaps);
    Ok(Some(claim.join("; ")))
}

/// Whether any way in a `LocationsOnWays` input carries inline coordinates:
/// the reference that keeps them compared with the one that drops them (the
/// same stable partition when osmium reads the input out of order, so only
/// the coordinates differ). Any unmatched object means the two references
/// are not one sequence, an error; the differing records are the objects
/// whose OPL line, way coordinates included, changed.
fn observed_coordinates(
    harness: &mut Harness,
    outdir: &Path,
    refs: &mut References<'_>,
    without: &Path,
) -> Result<bool, Error> {
    harness.msg("--- degrade: way coordinates in the input (OPL comparison) ---");
    let with = refs.get(harness, true)?;
    let compared = reencode::compare(harness, outdir, "coordinates", &with, without)?;
    let s = compared.merged;
    if s.first_only != 0 || s.second_only != 0 {
        return Err(Error::Failed(format!(
            "the references with and without way coordinates differ beyond coordinates: \
             {} objects only with them, {} only without",
            s.first_only, s.second_only
        )));
    }
    harness.msg(&format!(
        "  {} objects carry way coordinates the reference without them lost",
        s.differs
    ));
    Ok(s.differs > 0)
}

fn strip_locations(run: &mut Run<'_>, reference: &Path) -> Result<(), Error> {
    run.harness.msg("--- degrade --strip-locations ---");
    let Some(out) = run.degrade("strip-locations", &["--strip-locations"])? else {
        return Ok(());
    };
    let label = "degrade --strip-locations";
    let compared = reencode::compare(run.harness, &run.outdir, "strip-locations", reference, &out)?;
    reencode::record_identical(run.harness, label, &compared)?;
    run.common(label, &out, run.input.sorted_claim)?;
    if run.input.indexed {
        let mut layout = LayoutCheck::open(&out)?;
        for kind in 0..3 {
            let mut emit = |n| layout.expect(kind, n);
            reencode::each_claim(&run.input.path, kind, |claim| {
                split(DEFAULT_CAP, claim.count, &mut emit)
            })?;
        }
        reencode::record_layout(run.harness, label, layout.finish()?);
    }
    Ok(())
}

fn drop_ids(run: &mut Run<'_>, reference: &Path) -> Result<(), Error> {
    let total = run.input.total();
    let n = MAX_DROP.min(total / 4);
    let mut outputs = Vec::new();
    for (stem, seed) in [
        ("drop-a", SEEDS[0]),
        ("drop-a-again", SEEDS[0]),
        ("drop-b", SEEDS[1]),
    ] {
        run.harness
            .msg(&format!("--- degrade --drop-ids {n}:{seed} ---"));
        let spec = format!("{n}:{seed}");
        let Some(out) = run.degrade(stem, &["--drop-ids", &spec])? else {
            return Ok(());
        };
        outputs.push((stem, seed, out));
    }
    let mut removed_per_kind = [0u64; 3];
    for (stem, seed, out) in [&outputs[0], &outputs[2]] {
        let label = format!("degrade --drop-ids {n}:{seed}");
        let compared = reencode::compare(run.harness, &run.outdir, stem, reference, out)?;
        let per_kind = judge_drop(run, &label, &compared, n, total)?;
        for (most, removed) in removed_per_kind.iter_mut().zip(per_kind) {
            *most = (*most).max(removed);
        }
        run.common(&label, out, run.input.sorted_claim)?;
        if run.input.indexed {
            let mismatch = predict_drop(&run.input.path, &compared.listing, out)?;
            reencode::record_layout(run.harness, &label, mismatch);
        }
    }
    let again = reencode::compare(
        run.harness,
        &run.outdir,
        "drop-a-vs-again",
        &outputs[0].2,
        &outputs[1].2,
    )?;
    let same = again.identical();
    run.harness.msg(&format!(
        "  degrade --drop-ids {n}:{}: a second run {} {}",
        SEEDS[0],
        if same {
            "removed the same objects"
        } else {
            "differs"
        },
        if same { "PASS" } else { "FAIL" }
    ));
    run.harness.record(
        &format!("degrade --drop-ids {n}:{} repeatable", SEEDS[0]),
        Verdict::from_pass(same),
    );
    let seeds = reencode::compare(
        run.harness,
        &run.outdir,
        "drop-a-vs-b",
        &outputs[0].2,
        &outputs[2].2,
    )?;
    if seeds.identical() {
        run.gaps.push(format!(
            "--drop-ids seeds {} and {} removed the same set",
            SEEDS[0], SEEDS[1]
        ));
    }
    for (kind, removed) in removed_per_kind.iter().enumerate() {
        if *removed == 0 && run.input.info.counts[kind] > 0 {
            run.gaps
                .push(format!("--drop-ids removed no {}", KINDS[kind]));
        }
    }
    Ok(())
}

/// Hold one drop's diff to exactly `n` removed objects and nothing else;
/// returns the removed count per kind.
fn judge_drop(
    run: &mut Run<'_>,
    label: &str,
    compared: &Compared,
    n: u64,
    total: u64,
) -> Result<[u64; 3], Error> {
    let s = compared.merged;
    let ok = s.first_only == n && s.second_only == 0 && s.differs == 0 && s.same + n == total;
    if !ok {
        run.fail(
            label,
            &format!(
                "the comparison finds {} removed, {} added, {} same and {} changed, where \
                 exactly {n} of {total} objects must be removed and nothing else changed",
                s.first_only, s.second_only, s.same, s.differs
            ),
        );
        run.harness.replay_listing(&compared.listing)?;
    }
    let mut per_kind = [0u64; 3];
    let mut listing = Listing::open(&compared.listing)?;
    while let Some(group) = listing.next_group()? {
        if group.plus.is_some() || group.minus.is_none() {
            return Err(Error::Failed(format!(
                "{label}: the listing shows {}{} in the output",
                KINDS[group.kind], group.id
            )));
        }
        per_kind[group.kind] += 1;
    }
    listing.finish(&s)?;
    run.harness.msg(&format!(
        "  {label}: removed {} nodes, {} ways, {} relations{}",
        per_kind[0],
        per_kind[1],
        per_kind[2],
        if ok { " PASS" } else { "" }
    ));
    run.harness
        .record(&format!("{label} removals"), Verdict::from_pass(ok));
    Ok(per_kind)
}

/// Compare a drop output's blobs with each input blob re-encoded alone with
/// its survivors: the removed IDs, from the listing, counted per input blob
/// by its claimed range (the input in strict order, so ranges ascend and do
/// not overlap).
fn predict_drop(input: &Path, listing: &Path, output: &Path) -> Result<Option<String>, Error> {
    let mut layout = LayoutCheck::open(output)?;
    let mut removed = Listing::open(listing)?;
    let mut next = removed.next_group()?;
    for (kind, name) in KINDS.iter().enumerate() {
        let mut emit = |n| layout.expect(kind, n);
        reencode::each_claim(input, kind, |claim| {
            let mut gone = 0u64;
            while let Some(group) = &next {
                if group.kind != kind || group.id > claim.max_id {
                    break;
                }
                if group.id < claim.min_id {
                    return Err(Error::Failed(format!(
                        "removed {name}{} lies in no input blob's claimed range",
                        group.id
                    )));
                }
                gone += 1;
                next = removed.next_group()?;
            }
            let survivors = claim.count.checked_sub(gone).ok_or_else(|| {
                Error::Failed(format!(
                    "more objects removed from an input blob than its {} claimed",
                    claim.count
                ))
            })?;
            split(DEFAULT_CAP, survivors, &mut emit)
        })?;
        if let Some(group) = next.as_ref().filter(|g| g.kind == kind) {
            return Err(Error::Failed(format!(
                "removed {name}{} lies in no input blob's claimed range",
                group.id
            )));
        }
    }
    layout.finish()
}

fn unsort(run: &mut Run<'_>, reference: &Path, intra: bool) -> Result<(), Error> {
    let flag = if intra { "--unsort-intra" } else { "--unsort" };
    let stem = flag.trim_start_matches('-');
    let label = format!("degrade {flag}");
    run.harness.msg(&format!("--- {label} ---"));
    let Some(out) = run.degrade(stem, &[flag])? else {
        return Ok(());
    };
    let counts = run.input.info.counts;
    let fires: [bool; 3] = std::array::from_fn(|k| {
        if intra {
            counts[k] >= 2
        } else {
            counts[k] > DEFAULT_CAP
        }
    });
    let k = fires.iter().filter(|f| **f).count() as u64;
    if k == 0 {
        run.gaps.push(format!(
            "no kind holds enough objects for {flag} to move one"
        ));
    }
    let total = run.input.total();
    let compared = reencode::compare(run.harness, &run.outdir, stem, reference, &out)?;
    let s = compared.merged;
    if !(s.first_only == k && s.second_only == k && s.differs == 0 && s.same + k == total) {
        run.fail(
            &label,
            &format!(
                "the comparison finds {} and {} out of place, {} same and {} changed, where \
                 exactly {k} objects must be moved and nothing else changed",
                s.first_only, s.second_only, s.same, s.differs
            ),
        );
        run.harness.replay_listing(&compared.listing)?;
        return Ok(());
    }
    // The moved object per kind, both lines identical.
    let mut moved: [Option<i64>; 3] = [None; 3];
    let mut listing = Listing::open(&compared.listing)?;
    while let Some(group) = listing.next_group()? {
        let both_equal = group.minus.is_some() && group.minus == group.plus;
        if !both_equal || moved[group.kind].is_some() || !fires[group.kind] {
            run.fail(
                &label,
                &format!(
                    "{}{} is listed where one object per firing kind must move unchanged",
                    KINDS[group.kind], group.id
                ),
            );
            return Ok(());
        }
        moved[group.kind] = Some(group.id);
    }
    listing.finish(&s)?;
    let blobs = run.common(&label, &out, false)?;

    if !intra {
        let mut layout = LayoutCheck::open(&out)?;
        for (kind, &count) in counts.iter().enumerate() {
            let mut emit = |n| layout.expect(kind, n);
            continuous(DEFAULT_CAP, count, &mut emit)?;
        }
        reencode::record_layout(run.harness, &label, layout.finish()?);
    }
    for kind in 0..3 {
        let kb = &blobs.kinds[kind];
        let want_overlap = !intra && fires[kind];
        let overlap_ok = if want_overlap {
            kb.overlaps == 1 && kb.first_overlap == Some(0)
        } else {
            kb.overlaps == 0
        };
        if !overlap_ok {
            run.fail(
                &label,
                &format!(
                    "{} {}s blob pairs overlap where {} must",
                    kb.overlaps,
                    KINDS[kind],
                    if want_overlap {
                        "exactly the first pair"
                    } else {
                        "none"
                    }
                ),
            );
        }
        let Some(a) = moved[kind] else { continue };
        let placed = if intra {
            place_intra(&out, kb, kind, a, run.input.info.min_ids[kind])?
        } else {
            place_cross(&out, kb, kind, a)?
        };
        match placed {
            Ok(()) => run.harness.msg(&format!(
                "  {label}: {}{a} moved to its documented place PASS",
                KINDS[kind]
            )),
            Err(why) => run.fail(&label, &format!("{}{a}: {why}", KINDS[kind])),
        }
    }
    Ok(())
}

/// `--unsort`: `a` first in the kind's second blob, between the last two IDs
/// of its first, which holds `cap` elements.
fn place_cross(
    out: &Path,
    kb: &reencode::KindBlobs,
    kind: usize,
    a: i64,
) -> Result<Result<(), String>, Error> {
    let (Some(first), Some(second)) = (&kb.first, &kb.second) else {
        return Ok(Err("the kind has fewer than two output blobs".to_owned()));
    };
    if first.1.count != DEFAULT_CAP {
        return Ok(Err(format!(
            "the kind's first blob holds {} elements, not {DEFAULT_CAP}",
            first.1.count
        )));
    }
    let head = reencode::located_ids(out, first, kind)?;
    let tail = reencode::located_ids(out, second, kind)?;
    let (Some(&q), Some(&p)) = (head.last(), head.iter().rev().nth(1)) else {
        return Ok(Err(
            "the kind's first blob holds fewer than two elements".to_owned()
        ));
    };
    if tail.first() != Some(&a) {
        return Ok(Err(format!(
            "the second blob opens with {:?}, not the moved object",
            tail.first()
        )));
    }
    if !(p < a && a < q) {
        return Ok(Err(format!(
            "the first blob ends {p}, {q}, which the moved object does not fall between"
        )));
    }
    Ok(Ok(()))
}

/// `--unsort-intra`: `a` is the kind's lowest ID and second in its first
/// blob.
fn place_intra(
    out: &Path,
    kb: &reencode::KindBlobs,
    kind: usize,
    a: i64,
    lowest: i64,
) -> Result<Result<(), String>, Error> {
    if a != lowest {
        return Ok(Err(format!(
            "the moved object is not the kind's lowest ID {lowest}"
        )));
    }
    let Some(first) = &kb.first else {
        return Ok(Err("the kind has no output blob".to_owned()));
    };
    let head = reencode::located_ids(out, first, kind)?;
    if head.get(1) != Some(&a) {
        return Ok(Err(format!(
            "the first blob opens {:?}, the moved object not second",
            &head[..head.len().min(2)]
        )));
    }
    Ok(Ok(()))
}
