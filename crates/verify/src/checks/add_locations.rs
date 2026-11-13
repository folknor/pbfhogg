//! Verify: add-locations-to-ways - each pbfhogg node-index engine against
//! osmium, inline coordinates against pbfhogg's own consistency check, and
//! the engines against each other.

use std::path::{Path, PathBuf};

use crate::argv::PbfArgv;
use crate::error::Error;
use crate::harness::{Harness, Verdict};
use crate::reference::ToolRun;
use crate::verify_input_order::{self, InputOrder, Refusal, compare_outputs};
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub(crate) enum AltwMode {
    Sparse,
    External,
    All,
}

/// What a PASS of a single-engine run establishes. `pbfhogg diff` compares
/// way tags and refs but treats `LocationsOnWays` coordinates as derived data,
/// not content, so the osmium comparison never sees a coordinate; `check
/// --locations --agreement` holds each inline coordinate to the node record
/// the output keeps (most untagged nodes are dropped, so most references have
/// none) and to the other references of the same absent node. A wrong but
/// consistent coordinate for a dropped node passes both.
const SINGLE_CLAIM: &str = "element content and sort declaration match osmium; inline locations \
                            pass pbfhogg check --locations --agreement; inline locations are \
                            not compared with osmium";

/// [`SINGLE_CLAIM`] plus the engine parity `all` adds: per-kind `digest
/// --form full` records, which cover every inline coordinate exactly, agree
/// between the engines. Two engines writing the same wrong coordinate still
/// pass.
const ALL_CLAIM: &str = "element content and sort declaration match osmium; inline locations \
                         pass pbfhogg check --locations --agreement; sparse and external agree \
                         on full element records; inline locations are not compared with osmium";

/// [`SINGLE_CLAIM`] for the sparse engine on input without the sort claim:
/// osmium reads an osmium-sorted copy, and sparse's output must not claim
/// the sort and is compared through an osmium-sorted copy, whose strict
/// order proves it repeats no ID. The copy keeps no inline location, which
/// the diff does not read anyway; `check --locations` runs on the output
/// itself.
const SPARSE_UNSORTED_CLAIM: &str = "on unsorted input, against osmium on an osmium-sorted copy: \
                                     sparse's output claims no sort, repeats no ID and, through \
                                     an osmium-sorted copy, matches osmium's element content; \
                                     inline locations pass pbfhogg check --locations \
                                     --agreement; inline locations are not compared with osmium";

/// The external engine's command line, as a refusal claim names it.
const EXTERNAL_COMMAND: &str = "pbfhogg add-locations-to-ways --index-type external";

/// A node-index engine pbfhogg's `add-locations-to-ways --index-type` ships.
///
/// `auto` is not one: it routes to one of these two, its output is by
/// construction that engine's, and pbfhogg tests its routing. Verify names
/// the engine on every run rather than relying on pbfhogg's default, so a
/// change of default cannot silently change what is verified - the no-flag
/// run this replaced was labelled `hash` and had verified a different engine
/// before pbfhogg moved its default from the retired dense index to sparse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Engine {
    Sparse,
    External,
}

impl Engine {
    fn name(self) -> &'static str {
        match self {
            Self::Sparse => "sparse",
            Self::External => "external",
        }
    }
}

/// Cross-validate `pbfhogg add-locations-to-ways` against `osmium
/// add-locations-to-ways`.
///
/// `mode` selects the engines: `Sparse` or `External` runs that one, `All`
/// runs both and also requires them to agree on full element records.
///
/// The input header's `Sort.Type_then_ID` claim picks each engine's route
/// (`verify_input_order`). Sparse takes any order: on input without the
/// claim osmium reads an osmium-sorted copy and sparse's output is compared
/// through one. External's documented contract refuses such input, so there
/// it is held to that refusal and never compared; under `--mode external`
/// osmium does not run at all, and under `all` the engines are not compared
/// with each other. A header that claims the flag over disordered content
/// takes the ordinary route.
///
/// Every engine run is launched before any check reads an output, so one
/// engine's refusal or a later check's error never keeps the other from
/// running. On the ordinary route an engine that exits non-zero is a
/// recorded failure, never a skip and never read as anything more specific:
/// brokkr does not re-derive pbfhogg's other input preconditions (external
/// needs indexed input without negative node IDs; `check --locations`
/// refuses a history file), so an input outside that domain fails the check
/// rather than passing it. Each successful output is compared with osmium's
/// ([`compare_outputs`]) and goes through `check --locations --agreement`.
pub fn run(
    harness: &mut Harness,
    pbf: &Path,
    mode: AltwMode,
    direct_io: bool,
) -> Result<Option<String>, Error> {
    let outdir = harness.subdir("add-locations-to-ways")?;

    harness.msg("=== verify add-locations-to-ways ===");

    let sorted = verify_input_order::input_header(harness, pbf, &outdir)?.optional_sorted();
    if !sorted && matches!(mode, AltwMode::External) {
        // Nothing runs against osmium here, so no reference input is made.
        let claim = verify_input_order::refusal_claim(EXTERNAL_COMMAND, "", direct_io);
        hold_external_refusal(harness, pbf, &outdir, direct_io)?;
        return Ok(Some(claim));
    }
    let order = InputOrder::from_claim(harness, pbf, &outdir, sorted)?;

    // Osmium runs once as the reference for every engine. pbfhogg always
    // keeps untagged relation-member nodes (a documented deviation in
    // pbfhogg's DEVIATIONS.md); osmium does so only with --keep-member-nodes.
    let osmium_out = outdir.join("osmium.osm.pbf");
    let captured = harness.reference(
        &ToolRun::osmium("add-locations-to-ways.1")
            .args(&["add-locations-to-ways", "--keep-member-nodes"])
            .input(&order.reference_input)
            .arg("-o")
            .output(&osmium_out)
            .arg("--overwrite"),
        &outdir.join("osmium.stdout"),
    )?;
    harness.require_success(&captured, "osmium add-locations-to-ways")?;
    harness.print_inspect("osmium", &osmium_out)?;

    let (engines, claim): (&[Engine], String) = match (mode, sorted) {
        (AltwMode::Sparse, true) => (&[Engine::Sparse], SINGLE_CLAIM.to_owned()),
        (AltwMode::External, true) => (&[Engine::External], SINGLE_CLAIM.to_owned()),
        (AltwMode::All, true) => (&[Engine::Sparse, Engine::External], ALL_CLAIM.to_owned()),
        (AltwMode::Sparse, false) => (&[Engine::Sparse], SPARSE_UNSORTED_CLAIM.to_owned()),
        // `--mode external` on claimless input returned above.
        (AltwMode::External | AltwMode::All, false) => (
            &[Engine::Sparse, Engine::External],
            format!(
                "{SPARSE_UNSORTED_CLAIM}; external held to its documented refusal: {}; the \
                 engines are not compared with each other",
                verify_input_order::refusal_terms(EXTERNAL_COMMAND, "", direct_io)
            ),
        ),
    };

    // Every engine runs before any check, so a check's `Err` cannot keep an
    // engine from being exercised.
    let mut produced: Vec<(Engine, PathBuf)> = Vec::new();
    for &engine in engines {
        if !sorted && engine == Engine::External {
            hold_external_refusal(harness, pbf, &outdir, direct_io)?;
            continue;
        }
        let out_path = output_path(&outdir, engine);
        if run_engine(harness, pbf, &out_path, engine, direct_io)? {
            produced.push((engine, out_path));
        }
    }

    for (engine, out_path) in &produced {
        check_output(harness, &order, osmium_out.as_path(), out_path, *engine)?;
    }

    if let [(a, a_out), (b, b_out)] = produced.as_slice() {
        let verdict = engine_parity(harness, (*a, a_out), (*b, b_out))?;
        harness.record(
            &format!("{} vs {} full element records", a.name(), b.name()),
            verdict,
        );
    }

    Ok(Some(claim))
}

fn output_path(outdir: &Path, engine: Engine) -> PathBuf {
    outdir.join(format!("pbfhogg-{}.osm.pbf", engine.name()))
}

/// Hold the external engine to its documented refusal of input without the
/// sort claim (`verify_input_order::expect_unsorted_refusal`).
fn hold_external_refusal(
    harness: &mut Harness,
    pbf: &Path,
    outdir: &Path,
    direct_io: bool,
) -> Result<(), Error> {
    verify_input_order::expect_unsorted_refusal(
        harness,
        engine_argv(
            pbf,
            &output_path(outdir, Engine::External),
            Engine::External,
            direct_io,
        ),
        &Refusal {
            label: "add-locations-to-ways --index-type external",
            operand: pbf,
            answers: &[0],
            sink: None,
        },
    )
}

/// Run one engine. Returns whether it produced an output; a non-zero exit is
/// recorded on the harness with its stderr echoed. Launch errors and
/// interruption propagate.
fn run_engine(
    harness: &mut Harness,
    pbf: &Path,
    out_path: &Path,
    engine: Engine,
    direct_io: bool,
) -> Result<bool, Error> {
    let name = engine.name();
    harness.msg(&format!(
        "--- pbfhogg add-locations-to-ways --index-type {name} ---"
    ));
    let captured = harness.pbfhogg_discard(engine_argv(pbf, out_path, engine, direct_io))?;
    if captured.status.success() {
        return Ok(true);
    }
    harness.msg(&format!("  {name} engine FAILED (non-zero exit)"));
    for line in String::from_utf8_lossy(&captured.stderr).lines() {
        harness.msg(&format!("    {line}"));
    }
    harness.fail(format!(
        "pbfhogg add-locations-to-ways --index-type {name} exited non-zero"
    ));
    Ok(false)
}

/// The per-output checks: the comparison with osmium's output, and pbfhogg's
/// inline-location consistency check on the output itself (an osmium-sorted
/// copy keeps no inline location).
fn check_output(
    harness: &mut Harness,
    order: &InputOrder,
    osmium_out: &Path,
    out_path: &Path,
    engine: Engine,
) -> Result<(), Error> {
    let name = engine.name();
    harness.print_inspect(&format!("pbfhogg ({name})"), out_path)?;
    compare_outputs(
        harness,
        order,
        &format!("add-locations-to-ways --index-type {name}"),
        "osmium add-locations-to-ways",
        out_path,
        osmium_out,
    )?;
    let (verdict, _) = harness.check_locations(&format!("pbfhogg ({name})"), out_path, true)?;
    harness.record(&format!("{name} inline locations"), verdict);
    Ok(())
}

fn engine_argv(pbf: &Path, out_path: &Path, engine: Engine, direct_io: bool) -> PbfArgv {
    PbfArgv::new("add-locations-to-ways")
        .input(pbf)
        .output(out_path)
        .scratch()
        .arg("--index-type")
        .arg(engine.name())
        .arg_if(direct_io, "--direct-io")
}

/// Whether two engines' outputs agree on full element records: the node, way
/// and relation lines of `pbfhogg digest --form full`, which cover metadata
/// and exact way coordinates. The overall line's value is ignored on
/// purpose: it also covers the header, and header equality between engines is
/// not what this asserts. A digest that fails, prints a malformed report, or
/// reports a different form version from the other is an error, never a
/// verdict.
fn engine_parity(
    harness: &mut Harness,
    (a, a_out): (Engine, &Path),
    (b, b_out): (Engine, &Path),
) -> Result<Verdict, Error> {
    harness.msg(&format!(
        "--- engine parity ({} vs {}, digest --form full) ---",
        a.name(),
        b.name()
    ));
    let ours = digest_full(harness, a_out)?;
    let theirs = digest_full(harness, b_out)?;
    if ours.version != theirs.version {
        return Err(Error::Failed(format!(
            "pbfhogg digest reported form versions {} and {} for the two engine outputs; \
             their records are not comparable",
            ours.version, theirs.version
        )));
    }
    let differing = ours.differing_kinds(&theirs);
    if differing.is_empty() {
        harness.msg("  engine parity: PASS (node, way and relation records identical)");
        Ok(Verdict::Pass)
    } else {
        for kind in &differing {
            harness.msg(&format!("  {} {}: {}", a.name(), kind, ours.line(kind)));
            harness.msg(&format!("  {} {}: {}", b.name(), kind, theirs.line(kind)));
        }
        harness.msg(&format!(
            "  engine parity: FAIL ({} records differ)",
            differing.join(", ")
        ));
        Ok(Verdict::Fail)
    }
}

fn digest_full(harness: &mut Harness, pbf: &Path) -> Result<DigestReport, Error> {
    let sink = pbf.with_extension("digest.txt");
    let captured = harness.pbfhogg(
        PbfArgv::new("digest").arg("--form").arg("full").input(pbf),
        &sink,
    )?;
    harness.require_success(&captured, "pbfhogg digest --form full")?;
    DigestReport::parse(&crate::stream::read_report(&sink)?).map_err(|why| {
        Error::Failed(format!(
            "pbfhogg digest --form full printed an unexpected report for {}: {why}",
            pbf.display()
        ))
    })
}

/// The kinds a digest report carries, in the order it prints them.
const DIGEST_KINDS: [&str; 3] = ["node", "way", "relation"];

/// A parsed `pbfhogg digest` report: `xxh128 v<N> <hash>`, then exactly one
/// `<kind> <count> <hash>` line per kind, in order, and nothing else.
#[derive(Debug, PartialEq, Eq)]
struct DigestReport {
    version: u32,
    /// `<count> <hash>` per kind, in [`DIGEST_KINDS`] order.
    kinds: [String; 3],
}

impl DigestReport {
    fn parse(text: &str) -> Result<Self, String> {
        let mut lines = text.lines();
        let head = lines.next().ok_or("empty report")?;
        let mut head_fields = head.split(' ');
        if head_fields.next() != Some("xxh128") {
            return Err(format!("first line {head:?} does not name xxh128"));
        }
        let version = head_fields
            .next()
            .and_then(|v| v.strip_prefix('v'))
            .filter(|v| is_decimal(v))
            .and_then(|v| v.parse::<u32>().ok())
            .ok_or_else(|| format!("first line {head:?} carries no form version"))?;
        match (head_fields.next(), head_fields.next()) {
            (Some(hash), None) if is_hash(hash) => {}
            _ => {
                return Err(format!(
                    "first line {head:?} carries no single 128-bit hash"
                ));
            }
        }
        let mut kinds: [String; 3] = Default::default();
        for (slot, kind) in kinds.iter_mut().zip(DIGEST_KINDS) {
            let line = lines.next().ok_or_else(|| format!("no {kind} line"))?;
            let fields: Vec<&str> = line.split(' ').collect();
            match fields.as_slice() {
                [k, count, hash] if *k == kind && is_decimal(count) && is_hash(hash) => {
                    *slot = format!("{count} {hash}");
                }
                _ => return Err(format!("expected a {kind} line, found {line:?}")),
            }
        }
        // `lines()` already absorbs the final newline, so any further line,
        // blank or not, is a deviation.
        if let Some(extra) = lines.next() {
            return Err(format!("unexpected trailing line {extra:?}"));
        }
        Ok(Self { version, kinds })
    }

    /// The kinds whose records differ from `other`'s.
    fn differing_kinds(&self, other: &Self) -> Vec<&'static str> {
        DIGEST_KINDS
            .iter()
            .zip(self.kinds.iter().zip(&other.kinds))
            .filter(|(_, (a, b))| a != b)
            .map(|(kind, _)| *kind)
            .collect()
    }

    fn line(&self, kind: &str) -> &str {
        DIGEST_KINDS
            .iter()
            .position(|k| *k == kind)
            .map_or("", |i| self.kinds[i].as_str())
    }
}

/// Non-empty ASCII digits only: `str::parse` would also take a leading `+`.
fn is_decimal(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

fn is_hash(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::DigestReport;

    const H: &str = "0123456789abcdef0123456789abcdef";

    fn report(version: &str, way: &str) -> String {
        format!("xxh128 {version} {H}\nnode 3 {H}\nway {way} {H}\nrelation 0 {H}\n")
    }

    #[test]
    fn digest_report_parses_and_compares_by_kind() {
        let a = DigestReport::parse(&report("v2", "5")).unwrap();
        let b = DigestReport::parse(&report("v2", "6")).unwrap();
        assert_eq!(a.version, 2);
        assert!(a.differing_kinds(&a).is_empty());
        assert_eq!(a.differing_kinds(&b), vec!["way"]);
        assert_eq!(a.line("way"), format!("5 {H}"));
    }

    #[test]
    fn digest_report_refuses_malformed_output() {
        assert!(DigestReport::parse("").is_err());
        assert!(DigestReport::parse(&report("2", "5")).is_err());
        assert!(DigestReport::parse(&report("v2", "x")).is_err());
        assert!(DigestReport::parse(&report("v+2", "5")).is_err());
        assert!(DigestReport::parse(&report("v2", "+5")).is_err());
        // Trailing blank lines.
        assert!(DigestReport::parse(&format!("{}\n\n", report("v2", "5"))).is_err());
        // A missing kind line.
        assert!(DigestReport::parse(&format!("xxh128 v2 {H}\nnode 3 {H}\nway 5 {H}\n")).is_err());
        // Kinds out of order.
        assert!(
            DigestReport::parse(&format!(
                "xxh128 v2 {H}\nway 5 {H}\nnode 3 {H}\nrelation 0 {H}\n"
            ))
            .is_err()
        );
        // Anything after the relation line.
        assert!(DigestReport::parse(&format!("{}node 1 {H}\n", report("v2", "5"))).is_err());
        // A short hash.
        assert!(DigestReport::parse(&format!("xxh128 v2 abc\nnode 3 {H}\n")).is_err());
    }
}
