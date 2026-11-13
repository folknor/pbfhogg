//! Verify: repack - pbfhogg `repack` at the default cap, a cap that does not
//! divide the input's blobs (3000) and one that grows them (64000), and the
//! input stripped of indexdata repacked with `--force` at the default cap,
//! each held to the input's elements through osmium diff and to repack's
//! documented block layout (the shared rules and readers are
//! [`crate::verify_reencode`]).
//!
//! Every case must keep the element sequence, metadata and way coordinates
//! included (osmium diff against the input, or the stable partition of an
//! input whose kinds are not grouped); claim `Sort.Type_then_ID` and
//! `LocationsOnWays` as the input does; carry indexdata on every blob that
//! `check --index` finds true; hold no blob over the cap or empty, kinds in
//! order; and, on an indexed input, be laid out exactly as
//! [`RepackBuilder`] predicts from the input's claims. The stripped copy must
//! be unindexed and hold the input's data payloads byte for byte, or it is a
//! construction error; its layout is predicted from the original's claims.

use std::path::Path;

use crate::argv::PbfArgv;
use crate::error::Error;
use crate::harness::Harness;
use crate::pbf_frames::{self, Framing};
use crate::verify_reencode::{
    self as reencode, DEFAULT_CAP, LayoutCheck, Ran, References, RepackBuilder, Witnesses,
};

/// Fixed verification cases: a non-dividing cap and a growing cap.
const CAPS: [u64; 2] = [3000, 64000];

/// One repack run to judge.
struct Case<'a> {
    label: String,
    source: &'a Path,
    cap: Option<u64>,
    force: bool,
    /// Whose claims predict the layout, when any can.
    claims_of: Option<&'a Path>,
}

/// Run repack cross-validation (module doc).
#[allow(clippy::too_many_lines)]
pub fn run(harness: &mut Harness, pbf: &Path, direct_io: bool) -> Result<Option<String>, Error> {
    let outdir = harness.subdir("repack")?;
    harness.msg("--- repack: reading the input ---");
    let input = reencode::read_input(harness, &outdir, pbf)?;
    let mut refs = References::new(&outdir, &input);
    let reference = refs.get(harness, true)?;
    let mut gaps: Vec<String> = Vec::new();
    let mut refused: Vec<String> = Vec::new();
    let mut witnesses = Witnesses::default();
    let mut verified = 0u32;

    let claims_of = input.indexed.then_some(pbf);
    let mut cases = vec![Case {
        label: "default cap".to_owned(),
        source: pbf,
        cap: None,
        force: !input.indexed,
        claims_of,
    }];
    for cap in CAPS {
        cases.push(Case {
            label: format!("cap {cap}"),
            source: pbf,
            cap: Some(cap),
            force: !input.indexed,
            claims_of,
        });
    }
    let stripped = outdir.join("stripped.osm.pbf");
    if input.indexed {
        harness.msg("--- repack: deriving the input stripped of indexdata ---");
        harness.strip_indexdata(pbf, &stripped)?;
        if pbf_frames::framing(&stripped)? != Framing::Unindexed {
            return Err(Error::Failed(format!(
                "the stripped copy {} still carries indexdata, so it cannot stand for an \
                 unindexed input",
                stripped.display()
            )));
        }
        if let Err(why) = pbf_frames::same_data_payloads(pbf, &stripped)? {
            return Err(Error::Failed(format!(
                "the stripped copy {} does not hold the input's data payloads ({why}), so the \
                 input's claims cannot predict its layout",
                stripped.display()
            )));
        }
        cases.push(Case {
            label: "unindexed --force".to_owned(),
            source: &stripped,
            cap: None,
            force: true,
            claims_of: Some(pbf),
        });
    } else {
        gaps.push(
            "the input carries no indexdata, so no layout was predicted and the stripped case \
             did not run"
                .to_owned(),
        );
    }

    for case in &cases {
        harness.msg(&format!("--- repack: {} ---", case.label));
        let stem = case.label.replace(' ', "-");
        let out = outdir.join(format!("repack-{stem}.osm.pbf"));
        let mut argv = PbfArgv::new("repack");
        if let Some(cap) = case.cap {
            argv = argv.arg("--elements-per-blob").arg(cap.to_string());
        }
        let argv = argv
            .arg_if(case.force, "--force")
            .arg_if(direct_io, "--direct-io")
            .input(case.source)
            .output(&out);
        match reencode::reencode(harness, argv, &format!("pbfhogg repack ({})", case.label))? {
            Ran::TooLarge => {
                refused.push(case.label.clone());
                continue;
            }
            Ran::Written => verified += 1,
        }
        let label = format!("repack {}", case.label);
        let cap = case.cap.unwrap_or(DEFAULT_CAP);
        let compared = reencode::compare(harness, &outdir, &stem, &reference, &out)?;
        reencode::record_identical(harness, &label, &compared)?;
        reencode::judge_header(harness, &label, &out, input.sorted_claim, input.locations)?;
        reencode::judge_index(harness, &outdir, &label, &out)?;
        let blobs = reencode::output_blobs(&out, cap)?;
        reencode::record_blobs(harness, &label, &blobs);
        if let Some(claims_of) = case.claims_of {
            let mismatch = predict(claims_of, &out, cap, &mut witnesses)?;
            reencode::record_layout(harness, &label, mismatch);
        }
    }

    if verified == 0 {
        return Err(Error::Failed(
            "pbfhogg refused every repack as too large for the format: nothing was re-encoded, \
             so nothing was verified"
                .to_owned(),
        ));
    }
    if input.indexed {
        if !witnesses.remainder_then_grow {
            gaps.push(
                "no cap met a shrink blob's remainder followed by a smaller blob of its kind"
                    .to_owned(),
            );
        }
        if !witnesses.coalesced {
            gaps.push("no cap coalesced blobs smaller than it".to_owned());
        }
    }
    if !input.standard {
        gaps.push(
            "the input's kinds are not grouped in order: elements compared with osmium's stable \
             partition of it by kind"
                .to_owned(),
        );
    }
    for label in &refused {
        gaps.push(format!(
            "pbfhogg refused {label} as too large for the format; its representability and \
             correctness were not established"
        ));
    }
    let mut claim = vec![format!(
        "element sequence with metadata and way coordinates against osmium diff, header claims, \
         indexdata against payloads (check --index), cap and kind order{}",
        if input.indexed {
            ", and the documented block layout from the input's claims"
        } else {
            ""
        }
    )];
    claim.extend(gaps);
    Ok(Some(claim.join("; ")))
}

/// Compare `output`'s blobs with repack's rule run over the claims of
/// `claims_of`, kind by kind; the first disagreement.
fn predict(
    claims_of: &Path,
    output: &Path,
    cap: u64,
    witnesses: &mut Witnesses,
) -> Result<Option<String>, Error> {
    let mut layout = LayoutCheck::open(output)?;
    for kind in 0..3 {
        let mut builder = RepackBuilder::new(cap);
        {
            let mut emit = |n| layout.expect(kind, n);
            reencode::each_claim(claims_of, kind, |claim| {
                builder.blob(claim.count, &mut emit)
            })?;
            builder.finish(&mut emit)?;
        }
        witnesses.remainder_then_grow |= builder.witnesses.remainder_then_grow;
        witnesses.coalesced |= builder.witnesses.coalesced;
    }
    layout.finish()
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_caps_shrink_without_dividing_and_grow_the_default() {
        assert!(!super::CAPS.contains(&super::DEFAULT_CAP));
        assert_ne!(super::DEFAULT_CAP % super::CAPS[0], 0);
        assert!(super::CAPS[1] > super::DEFAULT_CAP);
    }
}
