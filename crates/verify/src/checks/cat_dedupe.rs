//! Verify: cat --dedupe - `pbfhogg cat --dedupe` held to the input itself.
//!
//! osmium merge is not an oracle here: it orders equal (type, ID, version)
//! copies by timestamp and writes the last, where pbfhogg keeps the earliest
//! input's copy and never reads the timestamp, and it refuses a repeated ID
//! and version inside one input. The check therefore works in a domain where
//! the survivor rule cannot matter: every merged input holds only copies of
//! the input's own objects, as osmium writes them, so any two copies of one
//! (type, ID, version) decode to the same OPL record, and the answer is the
//! input itself.
//!
//! On input claiming `Sort.Type_then_ID`, which must also be in strict kind
//! and ID order as osmium decodes it (a repeated ID or a disorder is outside
//! the domain, a verifier error), every run below must be identical to the
//! input as osmium decodes it, metadata always, inline coordinates when both
//! files declare them (`Policy::between`), and must claim the sort order:
//!
//! - `dedupe(A, A)`: every object a duplicate.
//! - `dedupe(A, B)`, B = `osmium tags-filter -R A nwr/amenity`: duplicate
//!   elimination where B overlaps A.
//! - `dedupe(B, C)` and `dedupe(C, B)`, C the same filter with
//!   `--invert-match`: a disjoint union, each operand supplying objects the
//!   other lacks. Both must hold an object, or the case is unexercised (a
//!   verifier error).
//! - `dedupe(A, W)` and `dedupe(W, A)`, W an `osmium getid` window chosen
//!   against A's own blobs ([`window`]): overlap runs that start and end
//!   between passthrough stretches, one of them bridging two A blobs that do
//!   not overlap each other.
//!
//! Every run's output is also held to the blob layout the documented plan
//! predicts from the inputs' framing as the verifier decodes it
//! ([`dedupe_plan`]): each passthrough blob's payload bytes in its place,
//! each run's re-encoded blobs within its ID span, the summary's blob counts
//! (pbfhogg's own account, beside the layout) and the `LocationsOnWays`
//! declaration (dropped exactly when a way blob is in a run). Byte equality
//! witnesses the preserved payload, not that nothing decoded it.
//!
//! B, C and W carry no indexdata (osmium writes none), so every run naming
//! them passes `--force` and takes the unindexed scan.
//!
//! Input without the claim is held to dedupe's Unsorted refusal of
//! `dedupe(A, A)` and nothing else runs. `--force` is passed on unindexed
//! input, whose refusal would otherwise be the missing indexdata.

use std::path::{Path, PathBuf};

use crate::argv::PbfArgv;
use crate::checks::dedupe_plan::{self, Entry, Segment};
use crate::error::Error;
use crate::harness::{Harness, Verdict, WayLocations, listing_beside};
use crate::opl_compare::{self, Policy};
use crate::pbf_frames::{self, Framing};
use crate::reference::ToolRun;
use crate::verify_input_order::{self, Refusal};

/// The selection splitting the input into two derived operands.
const SPLIT: &str = "nwr/amenity";

pub fn run(harness: &mut Harness, pbf: &Path, direct_io: bool) -> Result<Option<String>, Error> {
    let outdir = harness.subdir("cat-dedupe")?;
    harness.msg("=== verify cat --dedupe ===");
    let header = verify_input_order::input_header(harness, pbf, &outdir)?;
    let unindexed = pbf_frames::framing(pbf)? == Framing::Unindexed;

    if !header.optional_sorted() {
        let argv = PbfArgv::new("cat")
            .arg("--dedupe")
            .input(pbf)
            .input(pbf)
            .output(&outdir.join("refused.osm.pbf"))
            .arg_if(unindexed, "--force")
            .arg_if(direct_io, "--direct-io");
        verify_input_order::expect_unsorted_refusal(
            harness,
            argv,
            &Refusal {
                label: "cat --dedupe",
                operand: pbf,
                answers: &[0],
                sink: None,
            },
        )?;
        return Ok(Some(verify_input_order::refusal_claim(
            "pbfhogg cat --dedupe",
            ", merging the input with itself",
            direct_io,
        )));
    }

    let (ordered, _) = opl_compare::order(harness, pbf)?;
    if !ordered {
        return Err(Error::tool(
            "the input claims Sort.Type_then_ID but osmium does not decode it in strict kind and \
             ID order (a repeated ID or a disorder), outside the domain this check judges",
        ));
    }
    // The derived operands ask osmium for inline coordinates by the input's
    // declaration, never by payload arrays: an operand asked for them on an
    // undeclared input would declare LocationsOnWays, a merge of two such
    // operands would correctly keep it, and the judge, holding the merge to
    // the undeclared input, would fail a correct output. Passthrough blobs
    // are still held byte for byte to the operand fed, arrays included.
    let locations = header.optional_feature("LocationsOnWays");
    let input = dedupe_plan::entries(pbf)?;
    let a: Operand<'_> = (pbf, &input);

    merge(harness, &outdir, "self", pbf, [a, a], unindexed, direct_io)?;

    let tags = |invert: bool| {
        let mut args = vec![SPLIT.to_owned(), "-R".to_owned()];
        if invert {
            args.push("--invert-match".to_owned());
        }
        args
    };
    let split = "cat-dedupe-split.1";
    let selected = derive(
        harness,
        &outdir,
        pbf,
        "selected",
        (split, "tags-filter", tags(false)),
        locations,
    )?;
    let rest = derive(
        harness,
        &outdir,
        pbf,
        "rest",
        (split, "tags-filter", tags(true)),
        locations,
    )?;
    let (window_ids, windowed) = window(pbf, &input)?;
    let window = derive(
        harness,
        &outdir,
        pbf,
        "window",
        ("cat-dedupe-window.1", "getid", window_ids),
        locations,
    )?;
    let selected_entries = dedupe_plan::entries(&selected)?;
    let rest_entries = dedupe_plan::entries(&rest)?;
    let window_entries = dedupe_plan::entries(&window)?;
    let s: Operand<'_> = (&selected, &selected_entries);
    let c: Operand<'_> = (&rest, &rest_entries);
    let w: Operand<'_> = (&window, &window_entries);
    require_transitions(&input, &window_entries, &windowed)?;

    let cases: [(&str, [Operand<'_>; 2]); 5] = [
        ("input+selected", [a, s]),
        ("selected+rest", [s, c]),
        ("rest+selected", [c, s]),
        ("input+window", [a, w]),
        ("window+input", [w, a]),
    ];
    for (label, inputs) in cases {
        merge(harness, &outdir, label, pbf, inputs, true, direct_io)?;
    }

    Ok(Some(format!(
        "identical to the input as osmium decodes it (metadata included): merged with itself, \
         with its {SPLIT} selection, as the disjoint union of that selection and its \
         complement in both orders, and with a window of its own blobs in both orders; every \
         output blob in the documented passthrough or overlap-run place, the window \
         witnessing runs that start and end between passthrough stretches; the survivor rule \
         among differing copies is not established"
    )))
}

/// One merge input: its path and its decoded blobs.
type Operand<'a> = (&'a Path, &'a [Entry]);

/// The window must give each windowed kind one run flanked by passthroughs
/// of its kind, the node run (when nodes are windowed) joining two input
/// blobs, which never overlap each other in sorted input, so the run grew by
/// its widest reach. osmium's packing of the window is read, not assumed.
fn require_transitions(input: &[Entry], window: &[Entry], windowed: &[usize]) -> Result<(), Error> {
    let plan = dedupe_plan::predict(&[input, window]);
    let bridged = plan.iter().any(|seg| {
        matches!(seg, Segment::Run { kind: 0, members, .. }
            if members.iter().filter(|(from, _)| *from == 0).count() >= 2)
    });
    if dedupe_plan::shape(&plan).bounded_runs != windowed.len()
        || (windowed.contains(&0) && !bridged)
    {
        return Err(Error::tool(format!(
            "the window operand does not give one run flanked by passthroughs per windowed kind \
             (kinds {windowed:?}) with a node run bridging two input blobs, so the transitions \
             are unexercised"
        )));
    }
    Ok(())
}

/// The `osmium getid` IDs of the window operand, and the kinds it covers:
/// every second ID of the tail half of one interior node blob and of the
/// head half of the next (a bridge between two disjoint blobs), and every
/// second ID of one interior way blob and one interior relation blob. A
/// kind needs four blobs for the bridge and three for a single blob, so
/// each run has a passthrough of its kind on both sides; a kind with fewer
/// is not windowed, and an input with no windowed kind is outside the
/// domain.
fn window(pbf: &Path, entries: &[Entry]) -> Result<(Vec<String>, Vec<usize>), Error> {
    let mut ids = Vec::new();
    let mut kinds = Vec::new();
    for (kind, prefix) in [(0usize, 'n'), (1, 'w'), (2, 'r')] {
        let of_kind: Vec<&Entry> = entries.iter().filter(|e| e.kind == kind).collect();
        let n = of_kind.len();
        let picks: Vec<(usize, Option<bool>)> = if kind == 0 {
            if n < 4 {
                continue;
            }
            let k = n / 2 - 1;
            vec![(k, Some(true)), (k + 1, Some(false))]
        } else {
            if n < 3 {
                continue;
            }
            vec![(n / 2, None)]
        };
        for (at, tail) in picks {
            let blob = of_kind[at].blob;
            let decoded = pbf_frames::blob_ids(pbf, blob.payload_offset, blob.datasize)?;
            let half = decoded.len() / 2;
            let span = match tail {
                None => &decoded[..],
                Some(true) => &decoded[half..],
                Some(false) => &decoded[..half.max(1)],
            };
            ids.extend(
                span.iter()
                    .step_by(2)
                    .map(|(_, id)| format!("{prefix}{id}")),
            );
        }
        kinds.push(kind);
    }
    if kinds.is_empty() {
        return Err(Error::tool(
            "the input has too few blobs of every kind for a window flanked by passthroughs, \
             outside the domain this check judges",
        ));
    }
    Ok((ids, kinds))
}

/// An osmium selection of `pbf` (`tool`: the reference id, the osmium
/// command and its arguments after the input), held to carry the sort claim
/// (osmium-sorted when it does not), in strict order and holding at least
/// one object.
fn derive(
    harness: &mut Harness,
    outdir: &Path,
    pbf: &Path,
    name: &str,
    tool: (&str, &str, Vec<String>),
    locations: bool,
) -> Result<PathBuf, Error> {
    let (id, command, args) = tool;
    harness.msg(&format!("--- deriving {name} ---"));
    let filtered = outdir.join(format!("{name}-filtered.osm.pbf"));
    let mut run = ToolRun::osmium(id).arg(command).input(pbf);
    for arg in args {
        run = run.arg(arg);
    }
    if locations {
        run = run.args(&["-f", "pbf,locations_on_ways=true"]);
    }
    let captured = harness.reference(
        &run.arg("-o").output(&filtered).arg("--overwrite"),
        &outdir.join(format!("{name}-filtered.stdout")),
    )?;
    let what = format!("osmium {command}");
    harness.require_success(&captured, &what)?;
    if locations {
        crate::checks::sort::require_declared_locations(&filtered, &what)?;
    }
    let operand = if pbf_frames::header(&filtered)?.optional_sorted() {
        filtered
    } else {
        let dir = outdir.join(format!("{name}-sorted"));
        std::fs::create_dir_all(&dir)?;
        let sorted = dir.join(crate::checks::sort::OSMIUM_SORTED);
        crate::checks::sort::osmium_sort(harness, &filtered, &sorted, &dir.join("osmium.sink"))?;
        sorted
    };
    let (ordered, counts) = opl_compare::order(harness, &operand)?;
    harness.msg(&format!(
        "  {name}: {} nodes, {} ways, {} relations",
        counts[0], counts[1], counts[2]
    ));
    if !ordered {
        return Err(Error::tool(format!(
            "the derived operand {} is not in strict order, so it cannot stand for a sorted \
             subset of the input",
            operand.display()
        )));
    }
    if counts == [0; 3] {
        return Err(Error::tool(format!(
            "the derived operand {name} holds no object, so the disjoint union is unexercised"
        )));
    }
    Ok(operand)
}

/// One `pbfhogg cat --dedupe` of `inputs`, judged against `pbf` and against
/// the layout its plan predicts.
fn merge(
    harness: &mut Harness,
    outdir: &Path,
    label: &str,
    pbf: &Path,
    inputs: [Operand<'_>; 2],
    force: bool,
    direct_io: bool,
) -> Result<(), Error> {
    harness.msg(&format!("--- cat --dedupe {label} ---"));
    let plan = dedupe_plan::predict(&[inputs[0].1, inputs[1].1]);
    let shape = dedupe_plan::shape(&plan);
    harness.msg(&format!(
        "  predicted: {} blobs, {} passthrough, {} rewritten, {} runs between passthroughs",
        shape.total, shape.passthrough, shape.rewritten, shape.bounded_runs
    ));
    let out = outdir.join(format!("pbfhogg-{label}.osm.pbf"));
    let mut argv = PbfArgv::new("cat").arg("--dedupe");
    for (input, _) in inputs {
        argv = argv.input(input);
    }
    let argv = argv
        .output(&out)
        .arg_if(force, "--force")
        .arg_if(direct_io, "--direct-io");
    let captured = harness.pbfhogg_discard(argv)?;
    let name = format!("pbfhogg cat --dedupe {label}");
    harness.require_success(&captured, &name)?;
    let expect = if shape.way_rewritten {
        WayLocations::Dropped
    } else {
        WayLocations::Kept
    };
    harness.judge_way_locations(&name, pbf, &out, &captured, expect)?;

    let layout = dedupe_plan::judge(&out, &[inputs[0].0, inputs[1].0], &plan)?;
    harness.msg(&format!(
        "  {name}: blob layout {}",
        match &layout {
            Ok(()) => "as predicted PASS".to_owned(),
            Err(why) => format!("FAIL ({why})"),
        }
    ));
    harness.record(
        &format!("{name} output blobs in the predicted layout"),
        Verdict::from_pass(layout.is_ok()),
    );
    let counts = dedupe_plan::summary_counts(&captured.stderr_text());
    let want = (shape.total, shape.passthrough, shape.rewritten);
    let counted = counts == Some(want);
    harness.msg(&format!(
        "  {name}: summary blob counts {counts:?}, predicted {want:?} {}",
        if counted { "PASS" } else { "FAIL" }
    ));
    harness.record(
        &format!("{name} summary blob counts as predicted"),
        Verdict::from_pass(counted),
    );

    let claims = harness.has_sort_flag(&out)?;
    harness.msg(&format!(
        "  {name}: header {} Sort.Type_then_ID {}",
        if claims { "claims" } else { "does not claim" },
        if claims { "PASS" } else { "FAIL" }
    ));
    harness.record(
        &format!("{name} output claims Sort.Type_then_ID"),
        Verdict::from_pass(claims),
    );
    let policy = Policy::between(&out, pbf)?;
    let diff = harness.same_elements(&out, pbf, &listing_beside(&out), policy)?;
    harness.msg(&format!(
        "  {name}: {}",
        if diff.is_pass() {
            "PASS (identical to the input)"
        } else {
            "FAIL (differs from the input)"
        }
    ));
    harness.record(&format!("{name} vs input elements"), diff);
    Ok(())
}
