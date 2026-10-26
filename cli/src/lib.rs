//! The `pbfhogg` program. The binary (`cli/src/main.rs`) is only [`run`]; the
//! program is a library target so this package's tests can reach the command
//! tree production parses, [`cli_command`], without parsing `--help` text.
//! It is not an API: nothing outside this package should depend on it.

// Every file the CLI writes is a `pbfhogg::Output`: checked, staged and
// published by the library. clippy.toml lists the path-writing methods this
// refuses.
#![deny(clippy::disallowed_methods)]

mod command_paths;
mod exit_code;
mod host;
#[cfg(test)]
mod library_text_rule;
mod output;
mod signals;
#[cfg(test)]
mod test_scratch;

use command_paths::{CommandPaths, LeftoverScanner, parent_of, reject_aliases};
use exit_code::{Hinted, error_report, is_cancelled, is_platform_unsupported, output_hint};

use std::io::Write;
use std::path::PathBuf;
use std::process;

use clap::{Args, Parser, Subcommand, ValueEnum};
use pbfhogg::ElementKind;
use pbfhogg::altw::IndexType;
use pbfhogg::cat::CleanAttrs;
use pbfhogg::geocode_index::builder::BuildConfig as GeocodeBuildConfig;
use pbfhogg::getid::ElementRef;
use pbfhogg::tags_count::TagCountSort;
use pbfhogg::write::{OutputBackend, WriteOptions};
use pbfhogg::writer::Compression;
use pbfhogg::{HeaderOverrides, TypeFilter};

/// The failure exit statuses `pbfhogg` guarantees to callers. `0` and `1`
/// are a completed command's answer and travel as [`Outcome`], never through
/// the error channel.
///
/// The split follows the `diff`/`grep` convention: a command that ran to
/// completion and answered "no" is not the same event as a command that
/// could not run at all. Scripts can therefore branch on the answer
/// (`1`) without swallowing failures (`2`), which the previous
/// everything-is-`1` scheme made impossible.
///
/// `2` also matches what clap already returns for a usage error, so the
/// "the invocation did not work" band is contiguous.
///
/// `3` splits one cause out of that band: the host cannot provide a
/// requested I/O feature (`--direct-io`, `--io-uring`) at setup. Retrying
/// without the flag is the fix, which no other failure promises.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExitStatus {
    /// `2` - the command failed. Every error path but one lands here.
    Failed,
    /// `3` - the command failed because a requested platform I/O feature
    /// is unavailable at setup (`ErrorKind::PlatformUnsupported`). The
    /// error, naming the feature, goes to stderr as for `2`.
    PlatformUnsupported,
}

impl ExitStatus {
    const fn code(self) -> u8 {
        match self {
            Self::Failed => 2,
            Self::PlatformUnsupported => 3,
        }
    }
}

/// A command's completed answer, carried separately from errors so a
/// negative answer cannot be mistaken for a failed invocation: no wrapper
/// that re-boxes or maps an error can turn it into exit `2`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// `0` - the command ran to completion and its answer is positive (or
    /// the command has no yes/no answer).
    Positive,
    /// `1` - the command ran to completion and its answer is negative:
    /// `diff` found differences, `check` found violations,
    /// `inspect --indexed` found no index, `inspect --show` found no
    /// element. Not an error; nothing is printed to stderr for it.
    Negative,
}

impl Outcome {
    const fn code(self) -> u8 {
        match self {
            Self::Positive => 0,
            Self::Negative => 1,
        }
    }
}

fn accept_outcome(
    overall: &mut Outcome,
    result: Result<Outcome, Box<dyn std::error::Error>>,
) -> Result<(), Box<dyn std::error::Error>> {
    if result? == Outcome::Negative {
        *overall = Outcome::Negative;
    }
    Ok(())
}

/// Print a clap error (a usage error, or `--help` / `--version`) to the
/// stream clap would use and return clap's own exit code for it.
fn clap_exit(err: &clap::Error) -> process::ExitCode {
    let code = err.exit_code();
    drop(err.print());
    process::ExitCode::from(u8::try_from(code).unwrap_or(ExitStatus::Failed.code()))
}

#[derive(Parser)]
#[command(name = "pbfhogg", about = "OpenStreetMap PBF toolkit", version)]
struct Cli {
    /// The exit code is the answer: print only warnings and errors, no
    /// notes, progress or summary. Never changes what is written to -o or
    /// a report on stdout, except diff's text and check's report, which
    /// it drops. Accepted before or after the subcommand
    #[arg(short = 'q', long, global = true)]
    quiet: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Args)]
struct OutputArg {
    /// Output file
    #[arg(short, long)]
    output: PathBuf,
    #[command(flatten)]
    overwrite: OverwriteArg,
}

impl OutputArg {
    /// The output, checked against the output rules and staged, before any
    /// work is done. Cleanup trouble after a successful publication goes to
    /// the host's diagnostics.
    fn open(
        &self,
        host: &pbfhogg::host::Host,
    ) -> Result<pbfhogg::Output, Box<dyn std::error::Error>> {
        Ok(pbfhogg::Output::new(
            &self.output,
            self.overwrite.intent(),
            &host.diagnostics,
        )?)
    }
}

/// Where a command that needs scratch space puts it.
#[derive(Args)]
struct TmpDirArg {
    /// Directory for this run's scratch files, which can be large (add-locations-to-ways
    /// needs up to hundreds of GB with `--index-type external`; a parallel text diff has
    /// each shard write its full text output, unchanged lines included unless `-c`). The directory must exist and be writable; it is checked before any
    /// work starts. Each run makes one hidden `.pbfhogg-run-*` directory in it and removes it
    /// at the end. Default: the output's directory, or the current directory when there is
    /// no output file. Output staging files stay beside the output regardless.
    #[arg(long = "tmp-dir", value_name = "DIR")]
    tmp_dir: Option<PathBuf>,
}

#[derive(Args)]
struct OverwriteArg {
    /// Replace an output that already exists. Without it an existing output
    /// is refused before any work is done. A symlink is refused either way.
    #[arg(short = 'O', long)]
    overwrite: bool,
}

impl OverwriteArg {
    const fn intent(&self) -> pbfhogg::Overwrite {
        pbfhogg::Overwrite::from_flag(self.overwrite)
    }
}

/// `-O` for a command whose output is optional (stdout otherwise): without
/// `-o` there is no output to replace, so `-O` alone is refused.
#[derive(Args)]
struct OverwriteOutputArg {
    /// Replace an output that already exists. Without it an existing output
    /// is refused before any work is done. A symlink is refused either way.
    /// Requires -o: stdout is never replaced.
    #[arg(short = 'O', long, requires = "output")]
    overwrite: bool,
}

impl OverwriteOutputArg {
    const fn intent(&self) -> pbfhogg::Overwrite {
        pbfhogg::Overwrite::from_flag(self.overwrite)
    }
}

/// A command-line refusal found after clap parsed the arguments: a rule
/// clap cannot express (one that depends on a flag's value, or on the input
/// kind sniffed from the file), rendered by clap so it reads like clap's own
/// errors and exits with clap's usage status.
#[derive(Debug)]
struct UsageError(clap::Error);

impl std::fmt::Display for UsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0.render())
    }
}

impl std::error::Error for UsageError {}

/// A [`UsageError`] for the subcommand at `path` (`["inspect", "tags"]`).
fn usage(path: &[&str], message: impl std::fmt::Display) -> Box<dyn std::error::Error> {
    // Built first, so the subcommand's usage line carries its full name
    // (`pbfhogg inspect tags`).
    let mut cmd = cli_command();
    cmd.build();
    for name in path {
        let Some(sub) = cmd.find_subcommand(name).cloned() else {
            break;
        };
        cmd = sub;
    }
    Box::new(UsageError(
        cmd.error(clap::error::ErrorKind::ArgumentConflict, message),
    ))
}

/// The refusal of `-q, --quiet` beside a flag whose only effect is on output
/// a quiet run drops, `why` saying which. Worded like clap's own conflict
/// errors.
fn quiet_refused(path: &[&str], flag: &str, why: &str) -> Box<dyn std::error::Error> {
    usage(
        path,
        format!("the argument '--quiet' cannot be used with '{flag}': {why}"),
    )
}

/// A short flag a command used to have with a meaning that is not the
/// short's meaning in the one vocabulary every subcommand shares
/// (`reference/cli-reference.md`, "Short flags"). The letter stays reserved
/// on that command as a hidden argument whose only job is to refuse, naming
/// the long flag to spell instead, so an old command line never silently
/// means something else.
struct RetiredShort {
    /// Subcommand path, e.g. `["inspect", "tags"]`.
    path: &'static [&'static str],
    id: &'static str,
    short: char,
    /// The long flag the short used to stand for on this command.
    now: &'static str,
}

const RETIRED_SHORTS: &[RetiredShort] = &[
    RetiredShort {
        path: &["cat"],
        id: "retired-short-c",
        short: 'c',
        now: "--clean",
    },
    RetiredShort {
        path: &["extract"],
        id: "retired-short-c",
        short: 'c',
        now: "--config",
    },
    RetiredShort {
        path: &["extract"],
        id: "retired-short-s",
        short: 's',
        now: "--simple",
    },
    RetiredShort {
        path: &["renumber"],
        id: "retired-short-s",
        short: 's',
        now: "--start-id",
    },
    RetiredShort {
        path: &["getparents"],
        id: "retired-short-s",
        short: 's',
        now: "--add-self",
    },
    RetiredShort {
        path: &["inspect", "tags"],
        id: "retired-short-s",
        short: 's',
        now: "--sort",
    },
    RetiredShort {
        path: &["inspect"],
        id: "retired-short-e",
        short: 'e',
        now: "--extended",
    },
    RetiredShort {
        path: &["tags-filter"],
        id: "retired-short-t",
        short: 't',
        now: "--remove-tags",
    },
    RetiredShort {
        path: &["getid"],
        id: "retired-short-t",
        short: 't',
        now: "--remove-tags",
    },
    RetiredShort {
        path: &["tags-filter"],
        id: "retired-short-i",
        short: 'i',
        now: "--invert-match",
    },
];

/// The value parser of a retired short: it refuses whatever it is given.
#[derive(Clone)]
struct RetiredShortParser {
    short: char,
    now: &'static str,
}

impl clap::builder::TypedValueParser for RetiredShortParser {
    type Value = String;

    fn parse_ref(
        &self,
        cmd: &clap::Command,
        _arg: Option<&clap::Arg>,
        _value: &std::ffi::OsStr,
    ) -> Result<String, clap::Error> {
        Err(cmd.clone().error(
            clap::error::ErrorKind::UnknownArgument,
            format!(
                "-{} no longer stands for {} on this command (every short flag now has \
                 one meaning across all subcommands); spell it {}",
                self.short, self.now, self.now
            ),
        ))
    }
}

impl RetiredShort {
    fn arg(&self) -> clap::Arg {
        // `Set` with an optional value: the parser runs whether the letter is
        // given bare (`-s`) or with the value it used to take (`-s 5`), and
        // never when the letter is absent (no default value is parsed).
        clap::Arg::new(self.id)
            .short(self.short)
            .hide(true)
            .action(clap::ArgAction::Set)
            .num_args(0..=1)
            .default_missing_value("")
            .value_parser(RetiredShortParser {
                short: self.short,
                now: self.now,
            })
    }
}

fn mut_subcommand_at(
    cmd: clap::Command,
    path: &[&str],
    f: &dyn Fn(clap::Command) -> clap::Command,
) -> clap::Command {
    match path {
        [] => f(cmd),
        [name, rest @ ..] => cmd.mut_subcommand(*name, |sub| mut_subcommand_at(sub, rest, f)),
    }
}

/// A long flag renamed to a different spelling. The old spelling stays as a
/// hidden argument whose only job is to refuse, naming the new flag.
struct RetiredLong {
    path: &'static [&'static str],
    id: &'static str,
    long: &'static str,
    now: &'static str,
}

const RETIRED_LONGS: &[RetiredLong] = &[RetiredLong {
    path: &["getid"],
    id: "retired-long-verbose-ids",
    long: "verbose-ids",
    now: "-v, --verbose",
}];

#[derive(Clone)]
struct RetiredLongParser {
    long: &'static str,
    now: &'static str,
}

impl clap::builder::TypedValueParser for RetiredLongParser {
    type Value = String;

    fn parse_ref(
        &self,
        cmd: &clap::Command,
        _arg: Option<&clap::Arg>,
        _value: &std::ffi::OsStr,
    ) -> Result<String, clap::Error> {
        Err(cmd.clone().error(
            clap::error::ErrorKind::UnknownArgument,
            format!(
                "--{} was renamed (every command spells \"say more\" the same way); \
                 spell it {}",
                self.long, self.now
            ),
        ))
    }
}

impl RetiredLong {
    fn arg(&self) -> clap::Arg {
        clap::Arg::new(self.id)
            .long(self.long)
            .hide(true)
            .action(clap::ArgAction::Set)
            .num_args(0..=1)
            .default_missing_value("")
            .value_parser(RetiredLongParser {
                long: self.long,
                now: self.now,
            })
    }
}

/// The command tree production parses: the derived one, plus the retired
/// shorts and longs.
#[must_use]
pub fn cli_command() -> clap::Command {
    let mut cmd = <Cli as clap::CommandFactory>::command();
    for retired in RETIRED_LONGS {
        cmd = mut_subcommand_at(cmd, retired.path, &|sub| sub.arg(retired.arg()));
    }
    for retired in RETIRED_SHORTS {
        cmd = mut_subcommand_at(cmd, retired.path, &|sub| sub.arg(retired.arg()));
    }
    cmd
}

/// tags-filter's input kind and its checked expressions, as validation
/// resolved them.
type TagsFilterPlan = (InputKind, Vec<String>);

/// Whether `id` was given on the command line of subcommand `name`, as
/// opposed to taking its default.
fn given(matches: &clap::ArgMatches, name: &str, id: &str) -> bool {
    matches
        .subcommand_matches(name)
        .and_then(|sub| sub.value_source(id))
        == Some(clap::parser::ValueSource::CommandLine)
}

/// The flag rules clap cannot express, one function per command that has
/// any. They run before any input is opened, except tags-filter's OSC-mode
/// rules when the input kind is autodetected: those need the kind, which the
/// four-byte sniff (the read that has always decided it) provides.
///
/// The rule behind every refusal: a flag the command line itself makes
/// ineffective is refused. A fallback the host or the input selects at run
/// time (an unindexed input taking a sequential path, a one-core host) is
/// not grounds for refusal: the same command line would then be valid on one
/// machine or file and invalid on another.
///
/// Returns tags-filter's resolved input kind and checked expressions (the
/// expressions, and an `-e` file, are read before the kind sniff opens the
/// input).
fn validate_command(
    command: &Command,
    matches: &clap::ArgMatches,
    quiet: bool,
) -> Result<Option<TagsFilterPlan>, Box<dyn std::error::Error>> {
    match command {
        Command::TagsFilter { .. } => validate_tags_filter(command, matches).map(Some),
        Command::Diff { .. } => validate_diff(command, quiet).map(|()| None),
        // `-q` is global, so its conflicts are checked here rather than by
        // clap: a clap conflict on a global flag is not seen when the flag is
        // given before the subcommand.
        Command::Getid {
            verbose_ids: true, ..
        } if quiet => Err(quiet_refused(
            &["getid"],
            "--verbose",
            "it only prints the requested and missing IDs, which a quiet run drops",
        )),
        Command::AddLocationsToWays {
            index_type, force, ..
        } => {
            let index_type = index_type
                .resolve()
                .map_err(|e| usage(&["add-locations-to-ways"], e))?;
            if *force && index_type == IndexType::External {
                return Err(usage(
                    &["add-locations-to-ways"],
                    "--force has no effect with --index-type external, which requires indexed \
                     input whatever --force says (its ID bucket ranges come from per-blob \
                     indexdata)",
                ));
            }
            Ok(None)
        }
        Command::Degrade {
            unsort_intra,
            block_cap,
            ..
        } if *unsort_intra && block_cap.get() < 2 => Err(usage(
            &["degrade"],
            "--unsort-intra needs --block-cap >= 2: an intra-blob inversion requires at least \
             two same-kind elements inside one output block",
        )),
        Command::Check { .. } => validate_check(command, matches, quiet).map(|()| None),
        // A quiet `inspect --indexed` prints nothing: its answer is the exit
        // code, as for check and diff.
        Command::Inspect {
            indexed: true,
            json: true,
            ..
        } if quiet => Err(quiet_refused(
            &["inspect"],
            "--json",
            "it only shapes the --indexed verdict line, which a quiet run does not print",
        )),
        Command::MergeChanges { output, .. } => {
            validate_osc_output(&["merge-changes"], &output.output).map(|()| None)
        }
        _ => Ok(None),
    }
}

/// The name rule every OSC producer shares: `.osc` is written plain,
/// `.osc.gz` gzipped, anything else is a usage error before any work.
fn validate_osc_output(
    path: &[&str],
    output: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error>> {
    pbfhogg::osc::output_is_gzip(output)
        .map(|_| ())
        .map_err(|e| usage(path, e))
}

fn validate_tags_filter(
    command: &Command,
    matches: &clap::ArgMatches,
) -> Result<(InputKind, Vec<String>), Box<dyn std::error::Error>> {
    let Command::TagsFilter {
        file,
        expressions_file,
        expressions,
        input_kind,
        omit_referenced,
        invert_match,
        remove_tags,
        distrust,
        io,
        force,
        header,
        output,
        ..
    } = command
    else {
        return Ok((InputKind::Pbf, Vec::new()));
    };
    // The expressions first: they need no input, and the sniff below opens it.
    let expressions = filter_expressions(expressions_file.as_deref(), expressions)?;
    let kind = input_kind.unwrap_or_else(|| detect_input_kind(file));
    if matches!(kind, InputKind::Pbf) {
        return Ok((kind, expressions));
    }
    let how = if input_kind.is_some() {
        "selected by --input-kind osc"
    } else {
        "detected from the file; pass --input-kind pbf if it is a PBF"
    };
    // Each flag that only means something for a PBF: an OSC is XML, read by
    // one parser thread, with no blobs, no indexdata and no PBF header.
    let refused: [(bool, &str, &str); 9] = [
        (
            *omit_referenced,
            "-R/--omit-referenced",
            "OSC filtering keeps direct matches only",
        ),
        (
            *invert_match,
            "--invert-match",
            "the OSC filter has no inverted mode",
        ),
        (
            *remove_tags,
            "--remove-tags",
            "the OSC filter emits no referenced objects",
        ),
        (
            distrust.distrust_metadata,
            "--distrust-metadata",
            "OSC input carries no blob metadata",
        ),
        (
            given(matches, "tags-filter", "jobs"),
            "-j/--jobs",
            "the OSC parse is single-threaded",
        ),
        (
            header.is_set(),
            "--generator/--output-header",
            "OSC output has no PBF header",
        ),
        (
            given(matches, "tags-filter", "compression"),
            "--compression",
            "OSC output is not a PBF; its framing comes from the output name",
        ),
        (
            io.direct_io,
            "--direct-io",
            "the OSC path reads and writes through buffered I/O",
        ),
        (*force, "--force", "OSC input has no indexdata to require"),
    ];
    if let Some((_, flag, why)) = refused.iter().find(|(set, _, _)| *set) {
        return Err(usage(
            &["tags-filter"],
            format!("{flag} is not valid in OSC mode ({how}): {why}"),
        ));
    }
    validate_osc_output(&["tags-filter"], &output.output)?;
    Ok((kind, expressions))
}

fn validate_diff(command: &Command, quiet: bool) -> Result<(), Box<dyn std::error::Error>> {
    let Command::Diff {
        suppress_common,
        verbose,
        osmium_summary,
        output,
        type_filter,
        jobs,
        format,
        increment_version,
        update_timestamp,
        tmp,
        ..
    } = command
    else {
        return Ok(());
    };
    let refuse = |message: &str| Err(usage(&["diff"], message));
    match format {
        DiffFormat::Osc => {
            // Deliberately require output before rejecting text-only flags.
            // `-q` is not one: it is global, and a quiet OSC diff writes its
            // output like any other command, without the summary.
            let Some(output) = output else {
                return refuse("--output is required with --format osc");
            };
            for (set, flag) in [
                (*suppress_common, "--suppress-common"),
                (*verbose, "--verbose"),
                (*osmium_summary, "--osmium-summary"),
                (type_filter.is_some(), "--type"),
            ] {
                if set {
                    return refuse(&format!("{flag} is not valid with --format osc"));
                }
            }
            validate_osc_output(&["diff"], output)?;
        }
        DiffFormat::Text => {
            for (set, flag) in [
                (*increment_version, "--increment-version"),
                (*update_timestamp, "--update-timestamp"),
            ] {
                if set {
                    return refuse(&format!("{flag} is only valid with --format osc"));
                }
            }
            // `-j 1` runs one shard, whose text goes straight to the output:
            // no scratch. (`--format osc` keeps its scratch at one shard
            // too, so it keeps --tmp-dir.)
            if tmp.tmp_dir.is_some() && jobs.fixed() == Some(1) {
                return refuse(
                    "--tmp-dir has no effect with -j 1: a one-shard text diff writes no \
                     scratch files",
                );
            }
            // A quiet text diff discards its text and summary: its answer is
            // the exit code. The flags that only shape them are refused, and
            // so is --tmp-dir: a quiet diff only counts, sharded or not, and
            // counting writes no scratch.
            if quiet {
                for (set, flag, why) in [
                    (
                        output.is_some(),
                        "--output",
                        "a quiet text diff writes no diff text",
                    ),
                    (
                        tmp.tmp_dir.is_some(),
                        "--tmp-dir",
                        "a quiet text diff only counts, which writes no scratch files",
                    ),
                    (
                        *suppress_common,
                        "--suppress-common",
                        "it only shapes the diff text, which a quiet diff discards",
                    ),
                    (
                        *verbose,
                        "--verbose",
                        "it only shapes the diff text, which a quiet diff discards",
                    ),
                    (
                        *osmium_summary,
                        "--osmium-summary",
                        "it only shapes the summary, which a quiet diff drops",
                    ),
                ] {
                    if set {
                        return Err(quiet_refused(&["diff"], flag, why));
                    }
                }
            }
        }
    }
    Ok(())
}

fn validate_check(
    command: &Command,
    matches: &clap::ArgMatches,
    quiet: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let Command::Check {
        ids,
        refs,
        index,
        check_relations,
        show_ids,
        full,
        type_filter,
        json,
        ..
    } = command
    else {
        return Ok(());
    };
    // A quiet check prints no report: its answer is the exit code. The flags
    // that only shape the report are refused, first, as clap refused them
    // when `--quiet` was check's own flag.
    if quiet {
        for (set, flag) in [
            (*json, "--json"),
            (*show_ids, "--show-ids"),
            (given(matches, "check", "max_errors"), "--max-errors"),
        ] {
            if set {
                return Err(quiet_refused(
                    &["check"],
                    flag,
                    "it only shapes the report, which a quiet check does not print",
                ));
            }
        }
    }
    // With no selector the --ids + --refs pair runs; --index alone runs only
    // the metadata verification.
    let any_selector = *ids || *refs || *index;
    let run_ids = *ids || !any_selector;
    let run_refs = *refs || !any_selector;
    let refuse = |message: &str| Err(usage(&["check"], message));
    if !run_ids {
        if *full {
            return refuse("--full requires --ids (or omit --refs to run both checks)");
        }
        if type_filter.is_some() {
            return refuse("--type requires --ids (or omit --refs to run both checks)");
        }
    }
    if !run_refs {
        if *check_relations {
            return refuse("--check-relations requires --refs (or omit --ids to run both checks)");
        }
        if *show_ids {
            return refuse("--show-ids requires --refs (or omit --ids to run both checks)");
        }
    }
    Ok(())
}

/// Every free-form flag is parsed here at the argument boundary, by its
/// `value_parser`, into the typed value the library consumes, so a malformed
/// value is refused before any input is opened. Nothing downstream re-parses
/// a flag string: `run_*` takes `Compression`, `TypeFilter`, `Bbox`,
/// `StartIds` and friends, not `&str`. The exceptions are values that only
/// mean something together with another flag (positional IDs depend on
/// `--default-type`) or that live in a file the flag names (expression
/// files, `--polygon`, `--config`): those are checked by the command's
/// validation step, still before the command's main input is opened.
fn parse_type_filter(value: &str) -> Result<TypeFilter, String> {
    value.parse::<TypeFilter>().map_err(|e| e.to_string())
}

/// `--type` on a command that acts on one element kind: the shared
/// comma-list grammar, refusing a list of more than one kind.
fn parse_single_type(value: &str) -> Result<TypeFilter, String> {
    let filter = parse_type_filter(value)?;
    if filter.count() != 1 {
        return Err(format!(
            "this command acts on one element type, so --type takes exactly one of \
             node, way, or relation (got '{value}')"
        ));
    }
    Ok(filter)
}

/// `--type` on export: the shared grammar, refusing `relation`, which export
/// cannot write. `node,way` is both kinds, i.e. no restriction.
fn parse_export_type(value: &str) -> Result<pbfhogg::export::ExportTypes, String> {
    let filter = parse_type_filter(value)?;
    if filter.relations {
        return Err(format!(
            "export cannot export relations, so --type takes node, way, or node,way \
             (got '{value}')"
        ));
    }
    Ok(match (filter.nodes, filter.ways) {
        (true, false) => pbfhogg::export::ExportTypes::NodesOnly,
        (false, true) => pbfhogg::export::ExportTypes::WaysOnly,
        _ => pbfhogg::export::ExportTypes::All,
    })
}

fn parse_compression(value: &str) -> Result<Compression, String> {
    value.parse::<Compression>().map_err(|e| e.to_string())
}

fn parse_bbox(value: &str) -> Result<pbfhogg::extract::Bbox, String> {
    pbfhogg::extract::parse_bbox(value).map_err(|e| e.to_string())
}

/// An element cap (`--elements-per-blob`, `--block-cap`): non-zero by type in
/// the library, so zero is refused here, where the flag can be named.
fn parse_element_cap(value: &str) -> Result<std::num::NonZeroUsize, String> {
    match value.parse::<usize>() {
        Ok(cap) => std::num::NonZeroUsize::new(cap).ok_or_else(|| "must be > 0".to_owned()),
        Err(_) => Err(format!("'{value}' is not an element count")),
    }
}

fn parse_drop_ids(value: &str) -> Result<pbfhogg::degrade::DropSpec, String> {
    pbfhogg::degrade::DropSpec::parse(value).map_err(|e| e.to_string())
}

fn parse_output_header(value: &str) -> Result<pbfhogg::OutputHeaderEntry, String> {
    value
        .parse::<pbfhogg::OutputHeaderEntry>()
        .map_err(|e| e.to_string())
}

/// `time-filter`'s cutoff: UNIX seconds or an RFC 3339 UTC timestamp.
fn parse_timestamp(input: &str) -> Result<i64, String> {
    if let Ok(value) = input.parse::<i64>() {
        return Ok(value);
    }
    pbfhogg::commands::parse_rfc3339_utc(input).map_err(|e| format!("{e} (or UNIX seconds)"))
}

/// `renumber --start-id`: one value for all three kinds, or
/// `node,way,relation`.
#[derive(Clone, Copy, Debug)]
struct StartIds {
    node: i64,
    way: i64,
    relation: i64,
}

fn parse_start_ids(value: &str) -> Result<StartIds, String> {
    let parse = |part: &str, what: &str| -> Result<i64, String> {
        part.trim()
            .parse()
            .map_err(|_| format!("invalid {what}start ID: {part}"))
    };
    let parts: Vec<&str> = value.split(',').collect();
    match parts[..] {
        [one] => {
            let id = parse(one, "")?;
            Ok(StartIds {
                node: id,
                way: id,
                relation: id,
            })
        }
        [node, way, relation] => Ok(StartIds {
            node: parse(node, "node ")?,
            way: parse(way, "way ")?,
            relation: parse(relation, "relation ")?,
        }),
        _ => {
            Err("must be a single value or 3 comma-separated values (node,way,relation)".to_owned())
        }
    }
}

/// The one help text of `-j/--jobs`, on every command that has it.
const JOBS_HELP: &str = "Worker threads: 0 (default) sizes the worker pools automatically, \
     1 runs sequentially, N caps the worker pools at N threads. For diff, N is also the number \
     of ID-range shards";

/// `-j/--jobs`: `0` is auto, any other count a cap.
fn parse_jobs(value: &str) -> Result<pbfhogg::Parallelism, String> {
    value
        .parse::<usize>()
        .map(pbfhogg::Parallelism::from_jobs)
        .map_err(|_| format!("'{value}' is not a worker count (0 for automatic, or N >= 1)"))
}

/// The one help text of `--force`, on every command that has it.
const FORCE_HELP: &str = "Proceed on input without blob-level indexdata, which this command \
     would otherwise refuse, through its slower fallback path";

/// A switch that asks for a platform feature this build may lack. Refused
/// while the arguments are parsed, as a usage error, rather than when a
/// reader or writer is first opened - which on some commands comes minutes
/// into the run.
#[derive(Clone)]
struct CompiledSwitch {
    backend: OutputBackend,
    flag: &'static str,
    feature: &'static str,
}

impl clap::builder::TypedValueParser for CompiledSwitch {
    type Value = bool;

    fn parse_ref(
        &self,
        cmd: &clap::Command,
        _arg: Option<&clap::Arg>,
        value: &std::ffi::OsStr,
    ) -> Result<bool, clap::Error> {
        let on = value == "true";
        if on && !self.backend.is_compiled() {
            return Err(cmd.clone().error(
                clap::error::ErrorKind::ArgumentConflict,
                format!(
                    "{} requires the {} feature, which this build was compiled without",
                    self.flag, self.feature
                ),
            ));
        }
        Ok(on)
    }
}

const fn direct_io_switch() -> CompiledSwitch {
    CompiledSwitch {
        backend: OutputBackend::Direct,
        flag: "--direct-io",
        feature: "linux-direct-io",
    }
}

const fn io_uring_switch() -> CompiledSwitch {
    CompiledSwitch {
        backend: OutputBackend::Uring,
        flag: "--io-uring",
        feature: "linux-io-uring",
    }
}

#[derive(Args)]
struct CompressionArg {
    /// Compression: none, zlib (default), zstd. Append :LEVEL for custom (e.g. zlib:9, zstd:19)
    #[arg(long, default_value = "zlib", value_parser = parse_compression)]
    compression: Compression,
}

#[derive(Args)]
struct DirectIoArg {
    /// Use O_DIRECT to bypass page cache (requires linux-direct-io feature)
    #[arg(long, value_parser = direct_io_switch())]
    direct_io: bool,
}

#[derive(Args)]
struct DistrustMetadataArg {
    /// Ignore per-blob sidecar metadata (indexdata, tag index) and decode every
    /// blob. Slower, but the answer then depends only on element data.
    #[arg(long = "distrust-metadata")]
    distrust_metadata: bool,
}

#[derive(Args)]
struct ForceArg {
    #[arg(long, help = FORCE_HELP)]
    force: bool,
}

#[derive(Args)]
struct UringArg {
    /// Use io_uring for output I/O (requires linux-io-uring feature)
    #[arg(long, value_parser = io_uring_switch())]
    io_uring: bool,
}

#[derive(Args)]
struct HeaderOverrideArg {
    /// Override the writing program name in the output header
    #[arg(long)]
    generator: Option<String>,
    #[arg(
        long = "output-header",
        value_name = "KEY=VALUE",
        help = "Set output header fields.",
        long_help = pbfhogg::HeaderOverrides::output_header_help(),
        value_parser = parse_output_header
    )]
    output_headers: Vec<pbfhogg::OutputHeaderEntry>,
}

impl HeaderOverrideArg {
    fn overrides(self) -> HeaderOverrides {
        HeaderOverrides::from_entries(self.generator, self.output_headers)
    }

    fn is_set(&self) -> bool {
        self.generator.is_some() || !self.output_headers.is_empty()
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum DiffFormat {
    Text,
    Osc,
}

#[derive(Clone, Copy, ValueEnum)]
enum InputKind {
    Pbf,
    Osc,
}

#[derive(Clone, Copy, ValueEnum)]
enum DefaultTypeArg {
    Node,
    Way,
    Relation,
}

impl From<DefaultTypeArg> for ElementKind {
    fn from(arg: DefaultTypeArg) -> Self {
        match arg {
            DefaultTypeArg::Node => Self::Node,
            DefaultTypeArg::Way => Self::Way,
            DefaultTypeArg::Relation => Self::Relation,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum ExportFormatArg {
    Geojsonseq,
    Geojson,
}

/// Metadata attribute accepted by `--clean`. The set is closed, so it is
/// a `ValueEnum` rather than a string matched in a helper: `--help`
/// enumerates it and shell completion knows it.
#[derive(Clone, Copy, ValueEnum)]
enum CleanAttrArg {
    Version,
    Changeset,
    Timestamp,
    Uid,
    User,
}

fn clean_attrs(attrs: &[CleanAttrArg]) -> CleanAttrs {
    let mut clean = CleanAttrs::default();
    for attr in attrs {
        match attr {
            CleanAttrArg::Version => clean.version = true,
            CleanAttrArg::Changeset => clean.changeset = true,
            CleanAttrArg::Timestamp => clean.timestamp = true,
            CleanAttrArg::Uid => clean.uid = true,
            CleanAttrArg::User => clean.user = true,
        }
    }
    clean
}

/// Node-index strategy for `add-locations-to-ways`.
///
/// `Dense` is hidden rather than absent: it was a real value until it was
/// removed, and a user carrying an old command line deserves the pointer
/// at `sparse` instead of a bare "invalid value". Repository search cannot
/// establish whether downstream command lines still use it.
#[derive(Clone, Copy, ValueEnum)]
enum IndexTypeArg {
    Auto,
    Sparse,
    External,
    #[value(hide = true)]
    Dense,
}

impl IndexTypeArg {
    fn resolve(self) -> Result<IndexType, Box<dyn std::error::Error>> {
        match self {
            Self::Auto => Ok(IndexType::Auto),
            Self::Sparse => Ok(IndexType::Sparse),
            Self::External => Ok(IndexType::External),
            Self::Dense => "dense".parse::<IndexType>().map_err(|error| {
                format!("{error} For this command, pass --index-type sparse.").into()
            }),
        }
    }
}

/// `count` and `name` were accepted as shorthands by the hand-written
/// parser this replaced. They stay as hidden aliases because repository
/// search cannot establish whether downstream command lines still use them.
#[derive(Clone, Copy, ValueEnum)]
enum TagSortArg {
    #[value(alias = "count")]
    CountDesc,
    CountAsc,
    #[value(alias = "name")]
    NameAsc,
    NameDesc,
}

impl From<TagSortArg> for TagCountSort {
    fn from(value: TagSortArg) -> Self {
        match value {
            TagSortArg::CountDesc => Self::CountDesc,
            TagSortArg::CountAsc => Self::CountAsc,
            TagSortArg::NameAsc => Self::NameAsc,
            TagSortArg::NameDesc => Self::NameDesc,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum BenchReadMode {
    Sequential,
    Parallel,
    Pipelined,
    Blobreader,
}

#[derive(Clone, Copy, ValueEnum)]
enum BenchWriterMode {
    Sync,
    Pipelined,
}

#[derive(Clone, Copy, ValueEnum)]
enum BenchIoMode {
    Buffered,
    Uring,
}

#[derive(Subcommand)]
enum Command {
    /// Concatenate PBF files with optional type filtering
    #[command(group(
        clap::ArgGroup::new("cat_indexed_paths")
            .args(["type_filter", "dedupe"])
            .multiple(true)
    ))]
    Cat {
        /// Input PBF files
        #[arg(required = true)]
        files: Vec<PathBuf>,
        #[command(flatten)]
        output: OutputArg,
        /// Strip metadata attribute (version, timestamp, changeset, uid, user).
        /// Can be specified multiple times, e.g. --clean changeset --clean uid
        #[arg(long = "clean", value_name = "ATTR", conflicts_with = "dedupe")]
        clean: Vec<CleanAttrArg>,
        /// Filter by element type (comma-separated: node, way, relation)
        #[arg(
            short = 't',
            long = "type",
            value_parser = parse_type_filter,
            conflicts_with = "dedupe"
        )]
        type_filter: Option<TypeFilter>,
        /// Sorted k-way merge with dedup by (type, id). Requires sorted inputs.
        #[arg(long)]
        dedupe: bool,
        #[command(flatten)]
        compression: CompressionArg,
        #[command(flatten)]
        io: DirectIoArg,
        /// Use io_uring for output I/O (requires linux-io-uring feature).
        /// Only with --dedupe: plain cat has no io_uring writer
        #[arg(long, value_parser = io_uring_switch(), requires = "dedupe")]
        io_uring: bool,
        /// Proceed on input without blob-level indexdata, which --type and
        /// --dedupe would otherwise refuse, through the slower fallback path.
        /// Plain cat adds the indexdata and --clean decodes every blob
        /// anyway, so neither refuses, and --force needs --type or --dedupe
        #[arg(long, requires = "cat_indexed_paths")]
        force: bool,
        #[command(flatten)]
        header: HeaderOverrideArg,
    },
    /// Sort PBF into standard order (nodes → ways → relations, by ID)
    Sort {
        /// Input PBF file
        file: PathBuf,
        #[command(flatten)]
        output: OutputArg,
        #[command(flatten)]
        compression: CompressionArg,
        #[command(flatten)]
        io: DirectIoArg,
        #[command(flatten)]
        uring: UringArg,
        #[command(flatten)]
        force: ForceArg,
        #[command(flatten)]
        header: HeaderOverrideArg,
    },
    /// Re-encode a PBF with a different per-blob element cap.
    ///
    /// Element semantics, tags, refs, members, metadata, and DenseNodes
    /// encoding all round-trip. Output is type-sorted (nodes, ways,
    /// relations); the `Sort.Type_then_ID` flag is propagated when the
    /// input has it.
    ///
    /// Primary use case: producing same-corpus-different-encoding pairs
    /// for the blob-density measurement matrix. v1 only fires the cap
    /// on shrinks (cap < input blob size); growing across input blobs
    /// is deferred to v2.
    Repack {
        /// Input PBF file
        file: PathBuf,
        #[command(flatten)]
        output: OutputArg,
        /// Per-blob element cap. The default matches the PBF interop
        /// convention; passing a small cap against a `planet.openstreetmap.org`-
        /// packed input (~228 k/blob) produces a Geofabrik-style packing.
        #[arg(
            long = "elements-per-blob",
            default_value_t = pbfhogg::block_builder::DEFAULT_ELEMENT_CAP,
            value_parser = parse_element_cap
        )]
        elements_per_blob: std::num::NonZeroUsize,
        #[command(flatten)]
        compression: CompressionArg,
        #[command(flatten)]
        io: DirectIoArg,
        #[command(flatten)]
        uring: UringArg,
        #[command(flatten)]
        force: ForceArg,
        #[command(flatten)]
        header: HeaderOverrideArg,
    },
    /// Produce a valid-but-adversarial PBF by stripping properties or
    /// perturbing structure.
    ///
    /// At least one transformation flag is required. Flags compose, except
    /// `--unsort` and `--unsort-intra`, which are mutually exclusive.
    /// `--unsort` swaps elements across a block boundary when a kind has
    /// more than the block cap elements. `--unsort-intra` swaps the first
    /// two elements of each kind inside a block. Smaller kinds pass through.
    /// Used to produce inputs for benchmarking non-optimal code paths.
    #[command(group(
        clap::ArgGroup::new("degrade_transform")
            .args([
                "unsort", "unsort_intra", "strip_locations", "strip_indexdata",
                "strip_tagdata", "corrupt_tagdata", "corrupt_indexdata",
                "corrupt_indexdata_kind", "strip_bbox", "drop_ids",
            ])
            .multiple(true)
            .required(true)
    ))]
    #[command(group(
        clap::ArgGroup::new("degrade_decode")
            .args(["unsort", "unsort_intra", "strip_locations", "drop_ids"])
            .multiple(true)
    ))]
    Degrade {
        /// Input PBF file
        file: PathBuf,
        #[command(flatten)]
        output: OutputArg,
        /// Clear `Sort.Type_then_ID`; pack blobs to the cap so exactly one
        /// adjacent same-kind blob pair per kind has overlapping ID ranges
        /// (cross-blob overlap). Fires `sort`'s overlap-rewrite path.
        #[arg(long, conflicts_with = "unsort_intra")]
        unsort: bool,
        /// Clear `Sort.Type_then_ID`; leave one same-kind blob per kind with
        /// an internal ID-order inversion but non-overlapping blob ranges
        /// (intra-blob shape for `sort`'s monotonicity blind spot).
        #[arg(long = "unsort-intra")]
        unsort_intra: bool,
        /// Drop inline way-node coordinates (`LocationsOnWays`).
        #[arg(long)]
        strip_locations: bool,
        /// Clear `BlobHeader.indexdata` on every OsmData blob.
        #[arg(long)]
        strip_indexdata: bool,
        /// Clear `BlobHeader.tagdata` (the per-blob tag key index) on every
        /// OsmData blob, forcing `tags-filter`'s no-hint fallback path.
        /// Leaves `indexdata` intact - a tagdata-stripped file is still
        /// indexed.
        #[arg(long)]
        strip_tagdata: bool,
        /// Replace `BlobHeader.tagdata` on every OsmData blob with a
        /// well-formed tag key index naming one key the blob does not carry.
        /// The result parses cleanly and lies, so `tags-filter` skips blobs
        /// whose elements match - the adversarial case `--strip-tagdata`
        /// cannot produce, since a missing index is decoded rather than
        /// trusted. Header-only, so it cannot be combined with a
        /// blob-re-encoding flag.
        #[arg(
            long = "corrupt-tagdata",
            conflicts_with_all = ["strip_tagdata", "degrade_decode"]
        )]
        corrupt_tagdata: bool,
        /// Replace `BlobHeader.indexdata` on every OsmData blob with a
        /// well-formed index that overstates the blob: same element kind,
        /// inflated count, widened max ID. Produces the file `inspect`
        /// reports from without decoding anything, which is what
        /// `inspect --distrust-metadata` exists to see through.
        #[arg(
            long = "corrupt-indexdata",
            conflicts_with_all = ["strip_indexdata", "degrade_decode"]
        )]
        corrupt_indexdata: bool,
        /// Rotate the element *kind* in every OsmData blob's
        /// `BlobHeader.indexdata` - node to way to relation to node - leaving
        /// count and ID range honest. The kind is what per-kind blob
        /// schedules select on, so this is the corruption that makes a
        /// consumer skip a blob entirely rather than misreport it. Header-only.
        #[arg(
            long = "corrupt-indexdata-kind",
            conflicts_with_all = ["strip_indexdata", "degrade_decode"]
        )]
        corrupt_indexdata_kind: bool,
        /// Clear the `HeaderBlock.bbox` from the OSMHeader so the output
        /// declares no file-level bounding box. Header-only change; every
        /// OsmData blob passes through untouched.
        #[arg(long)]
        strip_bbox: bool,
        /// Drop exactly N elements chosen deterministically from their kind,
        /// ID, and SEED. Format: N:SEED (for example 5000:42).
        #[arg(long = "drop-ids", value_name = "N:SEED", value_parser = parse_drop_ids)]
        drop_ids: Option<pbfhogg::degrade::DropSpec>,
        /// Per-block element cap on the decode path. Hidden; used by
        /// the test suite so `--unsort` can fire on small fixtures
        /// without large fixtures per element kind.
        #[arg(
            long = "block-cap",
            default_value_t = pbfhogg::block_builder::DEFAULT_ELEMENT_CAP,
            value_parser = parse_element_cap,
            hide = true
        )]
        block_cap: std::num::NonZeroUsize,
        /// Proceed on input without blob-level indexdata, which the decode
        /// path (--unsort, --unsort-intra, --strip-locations, --drop-ids)
        /// would otherwise refuse; it falls back to scanning every blob for
        /// every kind. The header-only flags never refuse, so --force needs
        /// a decode-path flag
        #[arg(long, requires = "degrade_decode")]
        force: bool,
        #[command(flatten)]
        compression: CompressionArg,
        #[command(flatten)]
        io: DirectIoArg,
        #[command(flatten)]
        uring: UringArg,
        #[command(flatten)]
        header: HeaderOverrideArg,
    },
    /// Renumber all element IDs sequentially, remapping cross-references.
    ///
    /// Input must be sorted. Negative IDs (JOSM editor-local staging
    /// identifiers) are rejected - they must be resolved before renumbering.
    ///
    /// Unlike the other commands, the --compression default here is zlib:1
    /// (the measured external-renumber write path), not zlib:6. An explicit
    /// `--compression zlib` or `zlib:6` also yields level 1; any other
    /// value is honoured as given.
    Renumber {
        /// Input PBF file (must be sorted)
        file: PathBuf,
        #[command(flatten)]
        output: OutputArg,
        /// Starting ID(s): single value or comma-separated node,way,relation
        #[arg(long, default_value = "1", value_parser = parse_start_ids)]
        start_id: StartIds,
        #[command(flatten)]
        compression: CompressionArg,
        #[command(flatten)]
        io: DirectIoArg,
        #[command(flatten)]
        header: HeaderOverrideArg,
    },
    /// Filter elements by tag expressions.
    ///
    /// Default mode (without `-R`) resolves relation members transitively:
    /// matched relations pull in member ways, member nodes, nested member
    /// relations, and node refs of included ways.
    ///
    /// With `--input-kind osc`, filters an OSC change file instead, always
    /// preserving deletes. Flags that only mean something for a PBF are
    /// refused in OSC mode.
    #[command(group(
        clap::ArgGroup::new("tags_filter_expressions")
            .args(["expressions", "expressions_file"])
            .multiple(true)
            .required(true)
    ))]
    TagsFilter {
        /// Input file (PBF or OSC)
        file: PathBuf,
        #[command(flatten)]
        output: OutputArg,
        // Deliberately keep the extension-default help contract alongside the
        // four-byte sniff. Promising stricter content detection misstates fallback.
        /// Input kind override: pbf or osc (autodetect from extension by default)
        #[arg(long = "input-kind")]
        input_kind: Option<InputKind>,
        /// Omit referenced objects (faster, single pass, direct matches only)
        #[arg(short = 'R', long = "omit-referenced", conflicts_with_all = ["remove_tags", "jobs"])]
        omit_referenced: bool,
        /// Invert match: exclude matching objects, keep non-matching
        #[arg(long = "invert-match")]
        invert_match: bool,
        /// Remove tags from referenced objects not directly matched (not with -R)
        #[arg(long = "remove-tags")]
        remove_tags: bool,
        /// Read filter expressions from file (one per line, # comments)
        #[arg(short = 'e', long = "expressions")]
        expressions_file: Option<PathBuf>,
        /// Tag filter expressions (e.g. "highway=primary", "amenity", "w/building=yes")
        expressions: Vec<String>,
        #[command(flatten)]
        compression: CompressionArg,
        #[command(flatten)]
        io: DirectIoArg,
        /// Proceed on input without blob-level indexdata, which this command
        /// would otherwise refuse, through its slower fallback path. Not
        /// with --invert-match or --distrust-metadata, which never refuse
        #[arg(long, conflicts_with_all = ["invert_match", "distrust_metadata"])]
        force: bool,
        #[command(flatten)]
        distrust: DistrustMetadataArg,
        #[arg(
            short = 'j',
            long = "jobs",
            value_name = "N",
            default_value = "0",
            value_parser = parse_jobs,
            help = JOBS_HELP
        )]
        jobs: pbfhogg::Parallelism,
        #[command(flatten)]
        header: HeaderOverrideArg,
    },
    /// Export a PBF to GeoJSON or newline-delimited GeoJSONSeq.
    Export {
        /// Input PBF. LocationsOnWays is required for way export.
        file: PathBuf,
        /// Output file. Omit to write to stdout.
        #[arg(short, long)]
        output: Option<PathBuf>,
        #[command(flatten)]
        overwrite: OverwriteOutputArg,
        /// Output format: geojsonseq or geojson.
        #[arg(long = "format", default_value = "geojsonseq")]
        format: ExportFormatArg,
        /// Only export these element types (comma-separated: node, way;
        /// relations cannot be exported)
        #[arg(short = 't', long = "type", value_parser = parse_export_type)]
        type_filter: Option<pbfhogg::export::ExportTypes>,
        /// Read filter expressions from file, one per line.
        #[arg(short = 'e', long = "expressions")]
        expressions_file: Option<PathBuf>,
        /// Tag filter expressions, such as highway or w/building=yes.
        expressions: Vec<String>,
        /// Comma-separated whitelist of tag keys to retain.
        #[arg(long = "properties", value_delimiter = ',')]
        properties: Option<Vec<String>>,
        /// Spatial filter min_lon,min_lat,max_lon,max_lat. Way overlap is vertex-only and lossy.
        #[arg(short = 'b', long = "bbox", value_parser = parse_bbox)]
        bbox: Option<pbfhogg::extract::Bbox>,
        /// Include OSM metadata properties.
        #[arg(long = "metadata")]
        metadata: bool,
    },
    /// Compare two PBF files and show differences.
    ///
    /// Uses content equality (coordinates, tags, refs, members) - not version/timestamp
    /// ordering. This means diff output is deterministic regardless of whether metadata
    /// is present, partial, or absent in either input.
    Diff {
        /// Old PBF file
        old: PathBuf,
        /// New PBF file
        new: PathBuf,
        /// Hide unchanged elements (show only created/modified/deleted)
        #[arg(short = 'c', long = "suppress-common")]
        suppress_common: bool,
        /// Show detailed changes for modified elements
        #[arg(short = 'v', long)]
        verbose: bool,
        /// Print osmium-style summary (left/right/same/different
        /// counts) to stderr instead of the default pbfhogg-format
        /// summary. The pbfhogg-format summary always fires on
        /// stderr unless `--quiet` - this flag only swaps the
        /// format, it does not control whether a summary is shown.
        /// Not with -q, which drops the summary
        #[arg(short = 's', long = "osmium-summary")]
        osmium_summary: bool,
        /// Write diff output to file instead of stdout. Not with -q on a
        /// text diff, which writes no diff text
        #[arg(short = 'o', long = "output")]
        output: Option<PathBuf>,
        #[command(flatten)]
        overwrite: OverwriteOutputArg,
        /// Filter by element type (comma-separated: node, way, relation)
        #[arg(short = 't', long = "type", value_parser = parse_type_filter)]
        type_filter: Option<TypeFilter>,
        // Diff's N is a shard count as well as a thread cap: the block-pair
        // merge partitions the ID space into N shards, writes their text to
        // scratch and stitches it in order. Sharding needs both inputs
        // indexed and no negative IDs; otherwise the run uses one shard (or,
        // unindexed, the element stream) with a note, whatever -j says.
        #[arg(
            short = 'j',
            long = "jobs",
            value_name = "N",
            default_value = "0",
            value_parser = parse_jobs,
            help = JOBS_HELP
        )]
        jobs: pbfhogg::Parallelism,
        /// Output format: text (default) or osc
        #[arg(long, default_value = "text")]
        format: DiffFormat,
        /// Bump version of deleted elements by 1 (--format osc only)
        #[arg(long)]
        increment_version: bool,
        /// Set delete timestamp to current time (--format osc only)
        #[arg(long)]
        update_timestamp: bool,
        #[command(flatten)]
        tmp: TmpDirArg,
        #[command(flatten)]
        io: DirectIoArg,
    },
    /// Print a digest of a PBF's element content.
    ///
    /// Two files get the same digest exactly when `diff --suppress-common`
    /// reports them identical, whatever their encoding (compression, blob
    /// boundaries, dense or plain nodes, indexdata). Prints `xxh128 v1 HEX`,
    /// then `node|way|relation COUNT HEX` per kind. Refuses, printing
    /// nothing, any input on which diff's answer is not a function of
    /// element content: unsorted, out-of-order or duplicate IDs, mixed
    /// blocks, partial or inaccurate indexdata, unresolvable string
    /// references.
    Digest {
        /// Input PBF file
        file: PathBuf,
        #[command(flatten)]
        io: DirectIoArg,
    },
    /// Extract or remove elements by ID
    ///
    /// By default, keeps only the listed IDs. With `--invert`, removes the
    /// listed IDs and keeps everything else.
    #[command(group(
        clap::ArgGroup::new("getid_ids")
            .args(["ids", "id_file", "id_osm_file"])
            .multiple(true)
            .required(true)
    ))]
    Getid {
        /// Input PBF file
        file: PathBuf,
        #[command(flatten)]
        output: OutputArg,
        /// Invert selection: remove listed IDs instead of keeping them
        #[arg(long)]
        invert: bool,
        /// Include referenced nodes of matching ways (two-pass)
        #[arg(short = 'r', long = "add-referenced", conflicts_with = "invert")]
        add_referenced: bool,
        /// Remove tags from referenced objects not explicitly requested (requires -r)
        #[arg(long = "remove-tags", requires = "add_referenced")]
        remove_tags: bool,
        /// Print requested IDs and report which were not found
        #[arg(short = 'v', long = "verbose", conflicts_with = "invert")]
        verbose_ids: bool,
        /// Read IDs from text file (one per line, e.g. n123 or node/123)
        #[arg(short = 'i', long = "id-file")]
        id_file: Option<PathBuf>,
        /// Read IDs from an OSM/PBF file (all element IDs are collected)
        #[arg(short = 'I', long = "id-osm-file")]
        id_osm_file: Option<PathBuf>,
        /// Type for bare numeric IDs. Without this, each ID must name its type.
        #[arg(long = "default-type", value_enum)]
        default_type: Option<DefaultTypeArg>,
        /// Element IDs (e.g. n123 w456 r789, or node/123 way/456 relation/789)
        ids: Vec<String>,
        #[command(flatten)]
        compression: CompressionArg,
        #[command(flatten)]
        io: DirectIoArg,
        #[command(flatten)]
        force: ForceArg,
        #[command(flatten)]
        header: HeaderOverrideArg,
    },
    /// Find ways/relations referencing given IDs (reverse lookup)
    #[command(group(
        clap::ArgGroup::new("getparents_ids")
            .args(["ids", "id_file", "id_osm_file"])
            .multiple(true)
            .required(true)
    ))]
    Getparents {
        /// Input PBF file
        file: PathBuf,
        #[command(flatten)]
        output: OutputArg,
        /// Also include the queried objects themselves in the output
        #[arg(long = "add-self")]
        add_self: bool,
        /// Test instrument: override the ADR-0006 FullScan dispatch threshold.
        // Bounded at the CLI boundary rather than trusted raw into the
        // library entry point: 0 is not a threshold, it just pins the
        // FullScan arm unconditionally, and the upper bound keeps the
        // instrument inside the range a blob count can actually reach.
        #[arg(
            long,
            hide = true,
            value_parser = clap::value_parser!(u64).range(1..=1_000_000_000)
        )]
        full_scan_min_blobs: Option<u64>,
        /// Read IDs from text file (one per line, e.g. n123 or node/123)
        #[arg(short = 'i', long = "id-file")]
        id_file: Option<PathBuf>,
        /// Read IDs from an OSM/PBF file (all element IDs are collected)
        #[arg(short = 'I', long = "id-osm-file")]
        id_osm_file: Option<PathBuf>,
        /// Type for bare numeric IDs. Without this, each ID must name its type.
        #[arg(long = "default-type", value_enum)]
        default_type: Option<DefaultTypeArg>,
        /// Element IDs (e.g. n123 w456 r789, or node/123 way/456 relation/789)
        ids: Vec<String>,
        #[command(flatten)]
        compression: CompressionArg,
        #[command(flatten)]
        io: DirectIoArg,
        #[command(flatten)]
        header: HeaderOverrideArg,
    },
    /// Extract elements within a geographic region (bbox or polygon)
    Extract {
        /// Input PBF file
        file: PathBuf,
        /// Output file (required for single extract, omit with --config)
        #[arg(short, long, required_unless_present = "config")]
        output: Option<PathBuf>,
        /// Replace outputs that already exist, including every output a
        /// --config names. Without it any existing output is refused before
        /// any work is done. A symlink is refused either way.
        #[arg(short = 'O', long)]
        overwrite: bool,
        /// Bounding box: minlon,minlat,maxlon,maxlat
        #[arg(
            short = 'b',
            long,
            group = "area",
            required_unless_present_any = ["polygon", "config"],
            conflicts_with = "config",
            value_parser = parse_bbox
        )]
        bbox: Option<pbfhogg::extract::Bbox>,
        /// Polygon GeoJSON file
        #[arg(short = 'p', long, group = "area", conflicts_with = "config")]
        polygon: Option<PathBuf>,
        /// Multi-extract JSON config file
        #[arg(long, conflicts_with_all = ["bbox", "polygon", "output"])]
        config: Option<PathBuf>,
        /// Rebase config outputs under this directory (only with --config)
        #[arg(short = 'd', long, requires = "config")]
        directory: Option<PathBuf>,
        /// Simple strategy (single pass, may have dangling refs)
        #[arg(long, conflicts_with = "smart")]
        simple: bool,
        /// Smart strategy (three passes, complete multipolygon/boundary relations)
        #[arg(long, conflicts_with = "simple")]
        smart: bool,
        /// Write the extract region bounding box to the output header
        #[arg(long)]
        set_bounds: bool,
        /// Strip metadata attribute (version, timestamp, changeset, uid, user).
        /// Can be specified multiple times, e.g. --clean changeset --clean uid
        #[arg(long = "clean", value_name = "ATTR")]
        clean: Vec<CleanAttrArg>,
        #[command(flatten)]
        compression: CompressionArg,
        #[command(flatten)]
        io: DirectIoArg,
        /// Proceed on input without blob-level indexdata, which the
        /// complete-ways and smart strategies would otherwise refuse, through
        /// their slower fallback path. Not with --simple, which never refuses
        #[arg(long, conflicts_with = "simple")]
        force: bool,
        #[command(flatten)]
        header: HeaderOverrideArg,
    },
    /// Embed node coordinates in ways
    AddLocationsToWays {
        /// Input PBF file
        file: PathBuf,
        #[command(flatten)]
        output: OutputArg,
        /// Keep all untagged nodes in output (default: drop untagged nodes unless referenced by a relation)
        #[arg(long)]
        keep_untagged_nodes: bool,
        /// Node coordinate index type:
        ///   auto     - route by input eligibility and estimated node-store size:
        ///              unsorted or unindexed input uses sparse; sorted, indexed
        ///              input uses external when the estimate exceeds the
        ///              fixed share of the page-cache budget. If either
        ///              estimate is unavailable, sorted, indexed input uses
        ///              external. See reference/pipeline.md.
        ///   sparse   - rank-indexed flat mmap array, sized at 8 bytes per
        ///              referenced node. Works on any PBF and needs temporary
        ///              disk equal to referenced_nodes * 8 bytes. This is the default.
        ///   external - double radix join with bounded working memory and
        ///              sequential I/O. Requires sorted input with indexdata and
        ///              has high temporary-disk demand. See reference/performance.md.
        #[arg(long, default_value = "sparse")]
        index_type: IndexTypeArg,
        /// Emit pbfhogg's opt-in WayMembers-v1 and SharedNodePins-v1 metadata.
        #[arg(long)]
        inject_prepass: bool,
        /// Accept ways whose node references are missing from the input,
        /// writing those vertices as (0, 0). Without this the run fails:
        /// (0, 0) is Null Island, a real coordinate, so a placeholder vertex
        /// is indistinguishable from genuine data in the output.
        #[arg(long)]
        ignore_missing_nodes: bool,
        #[command(flatten)]
        compression: CompressionArg,
        #[command(flatten)]
        io: DirectIoArg,
        /// Proceed on input without blob-level indexdata, which this command
        /// would otherwise refuse, through its slower fallback path. Not with
        /// --inject-prepass or --index-type external, which need indexdata
        /// whatever --force says
        #[arg(long, conflicts_with = "inject_prepass")]
        force: bool,
        #[command(flatten)]
        header: HeaderOverrideArg,
        #[command(flatten)]
        tmp: TmpDirArg,
    },
    /// Filter history PBF to a snapshot at a timestamp
    TimeFilter {
        /// Input history PBF file
        file: PathBuf,
        #[command(flatten)]
        output: OutputArg,
        /// Snapshot cutoff timestamp (UNIX seconds or RFC3339 UTC: YYYY-MM-DDTHH:MM:SSZ)
        #[arg(value_parser = parse_timestamp)]
        timestamp: i64,
        #[command(flatten)]
        compression: CompressionArg,
        #[command(flatten)]
        io: DirectIoArg,
        #[command(flatten)]
        header: HeaderOverrideArg,
    },
    /// Inspect PBF file: metadata, block breakdown, ordering analysis
    #[command(subcommand_negates_reqs = true, args_conflicts_with_subcommands = true)]
    Inspect {
        #[command(subcommand)]
        subcommand: Option<InspectCommand>,
        /// Input PBF file
        #[arg(required = true)]
        file: Option<PathBuf>,
        /// Check if PBF has blob-level indexdata (exit code 0/1). Reads the
        /// first data blob only; later blobs are not checked.
        #[arg(long)]
        indexed: bool,
        /// Analyze node coordinate statistics for FOR compression sizing
        #[arg(long)]
        nodes: bool,
        // Only the `--nodes` scan has a worker pool, so `-j` is refused
        // outside `--nodes` rather than ignored.
        #[arg(
            short = 'j',
            long = "jobs",
            value_name = "N",
            default_value = "0",
            value_parser = parse_jobs,
            requires = "nodes",
            help = JOBS_HELP
        )]
        jobs: pbfhogg::Parallelism,
        /// Show per-block distribution stats and optional block listing
        #[arg(long, num_args = 0..=1, default_missing_value = "0")]
        blocks: Option<usize>,
        /// Show min/max element IDs per type and monotonicity
        #[arg(long)]
        id_ranges: bool,
        /// Show locations-on-ways diagnostics
        #[arg(long)]
        locations: bool,
        #[arg(
            long,
            help = "Show blocks outside inspect's per-type median thresholds, plus mixed blocks"
        )]
        anomalies: bool,
        /// Extended scan: timestamp range, data bbox, metadata coverage, ordering
        #[arg(long)]
        extended: bool,
        /// Show a single element by type/ID (e.g. "n123" or "node/123").
        ///
        /// A mode of its own: it returns one element and nothing else, so
        /// every other inspect flag is rejected alongside it rather than
        /// silently discarded (which is what used to happen).
        #[arg(
            long,
            value_name = "TYPE_ID",
            value_parser = clap::value_parser!(ElementRef),
            conflicts_with_all = [
                "indexed", "nodes", "jobs", "blocks", "id_ranges", "locations",
                "anomalies", "extended", "get", "json", "force",
                "distrust_metadata",
            ],
        )]
        show: Option<ElementRef>,
        /// Get a single value by key path (e.g. "header.bbox", "data.timestamp.first").
        /// Data and metadata paths enable extended stats automatically.
        /// Not with --blocks or --anomalies, which only shape the full report.
        #[arg(
            short,
            long,
            value_name = "KEY",
            conflicts_with_all = ["blocks", "anomalies"]
        )]
        get: Option<String>,
        /// Machine-readable JSON output. Not with --nodes (node stats have no
        /// JSON form) or --get
        #[arg(long, conflicts_with_all = ["nodes", "get"])]
        json: bool,
        #[command(flatten)]
        io: DirectIoArg,
        #[command(flatten)]
        distrust: DistrustMetadataArg,
        /// Proceed on input without blob-level indexdata, which the --nodes
        /// scan would otherwise refuse, through its slower fallback path.
        /// Only --nodes refuses, so --force requires --nodes
        #[arg(long, requires = "nodes")]
        force: bool,
    },
    /// Apply OSC diffs to a PBF file
    ApplyChanges {
        /// Base PBF file
        base: PathBuf,
        /// OSC diff file (.osc.gz)
        changes: PathBuf,
        #[command(flatten)]
        output: OutputArg,
        #[command(flatten)]
        compression: CompressionArg,
        #[command(flatten)]
        io: DirectIoArg,
        #[command(flatten)]
        uring: UringArg,
        /// Proceed on a base without blob-level indexdata, which this command
        /// would otherwise refuse, through its slower fallback path. Not with
        /// --locations-on-ways, which needs an indexed base whatever --force
        /// says
        #[arg(long, conflicts_with = "locations_on_ways")]
        force: bool,
        /// Preserve way-node locations through the merge. Ways the OSC
        /// creates or modifies get locations from the merged nodes; base
        /// ways the OSC does not touch keep their existing locations, even
        /// if the OSC moves or deletes a node they reference. Rerun
        /// add-locations-to-ways on the output for a full refresh.
        /// Requires the base PBF to have LocationsOnWays and be sorted.
        #[arg(long)]
        locations_on_ways: bool,
        /// Permit unresolved references to use the legacy (0,0) fallback.
        #[arg(long, requires = "locations_on_ways")]
        ignore_missing_nodes: bool,
        /// Accept a change whose version is lower than the one already
        /// held for that object. Off by default: an out-of-order diff
        /// otherwise downgrades objects to an older state and reports
        /// success. An equal version is always accepted; the later
        /// change wins.
        #[arg(long)]
        allow_version_regression: bool,
        // Auto keeps the `nproc - 2` heuristic (two cores left for the
        // scanner and drain threads, minimum 2 workers); `-j 1` runs one.
        #[arg(
            short = 'j',
            long = "jobs",
            value_name = "N",
            default_value = "0",
            value_parser = parse_jobs,
            help = JOBS_HELP
        )]
        jobs: pbfhogg::Parallelism,
        #[command(flatten)]
        header: HeaderOverrideArg,
    },
    /// Merge multiple OSC files into one OSC file
    MergeChanges {
        /// Input OSC files (.osc or .osc.gz)
        #[arg(required = true)]
        changes: Vec<PathBuf>,
        #[command(flatten)]
        output: OutputArg,
        /// Keep only the last change per object (type + id)
        #[arg(long)]
        simplify: bool,
        /// Accept an incoming change whose version is lower than the
        /// highest version already claimed for that object, deletes
        /// included. An equal version is always accepted, and the later
        /// change wins. Off by default: --simplify keeps the LAST
        /// change per object by input position, so inputs listed out of
        /// sequence order otherwise downgrade objects to older versions
        /// and report success. Requires --simplify: without it every change
        /// is kept in input order and no version is compared.
        #[arg(long, requires = "simplify")]
        allow_version_regression: bool,
        // Sizes both the per-input parse fan-out (capped by input count) and
        // the simplify path's `write_simplified` chunk fan-out.
        #[arg(
            short = 'j',
            long = "jobs",
            value_name = "N",
            default_value = "0",
            value_parser = parse_jobs,
            help = JOBS_HELP
        )]
        jobs: pbfhogg::Parallelism,
    },
    /// Validate PBF file integrity (IDs + referential integrity)
    Check {
        /// Input PBF file
        file: PathBuf,
        /// Check ID uniqueness and ordering
        #[arg(long)]
        ids: bool,
        /// Check referential integrity
        #[arg(long)]
        refs: bool,
        /// Verify blob-level metadata (BlobHeader indexdata + tagdata) against
        /// the blobs themselves. Decompresses every blob. Opt-in: not part of
        /// the default --ids + --refs pair.
        #[arg(long)]
        index: bool,
        /// Also check relation member references (use with --refs)
        #[arg(long)]
        check_relations: bool,
        /// Show IDs of missing objects (use with --refs)
        #[arg(long)]
        show_ids: bool,
        /// Full duplicate detection via bitmap (use with --ids)
        #[arg(long)]
        full: bool,
        /// Filter by element type for ID check (comma-separated: node, way, relation)
        #[arg(short = 't', long = "type", value_parser = parse_type_filter)]
        type_filter: Option<TypeFilter>,
        /// Retain at most N violation details per check (0 = unlimited);
        /// the scan still runs to the end and the counts stay exact
        #[arg(long, default_value = "100")]
        max_errors: usize,
        /// Machine-readable JSON output. Not with -q, which drops the report
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        io: DirectIoArg,
    },
    /// Benchmark: count elements using a single read mode (emits kv to stderr)
    BenchRead {
        /// Input PBF file
        file: PathBuf,
        /// Read mode: sequential, parallel, pipelined, blobreader
        #[arg(long)]
        mode: BenchReadMode,
    },
    /// Benchmark: decode + write all elements through BlockBuilder (emits kv to stderr)
    BenchWrite {
        /// Input PBF file
        file: PathBuf,
        /// Compression spec (e.g. none, zlib:6, zstd:3)
        #[arg(long, default_value = "zlib", value_parser = parse_compression)]
        compression: Compression,
        /// Writer mode: sync or pipelined
        #[arg(long, default_value = "sync")]
        writer: BenchWriterMode,
    },
    /// Benchmark: merge base PBF with OSC diff (emits kv to stderr)
    BenchMerge {
        /// Base PBF file
        base: PathBuf,
        /// OSC diff file (.osc.gz)
        changes: PathBuf,
        #[command(flatten)]
        output: OutputArg,
        /// Compression spec (e.g. none, zlib, zstd:3)
        #[arg(long, default_value = "zlib", value_parser = parse_compression)]
        compression: Compression,
        /// I/O mode: buffered, uring
        #[arg(long, default_value = "buffered")]
        io_mode: BenchIoMode,
    },
    /// Build a reverse geocoding index from a PBF file
    BuildGeocodeIndex {
        /// Input PBF file
        file: PathBuf,
        /// Output directory for index files
        #[arg(short = 'o', long)]
        output_dir: PathBuf,
        /// S2 cell level for streets/addresses; must exceed --coarse-level
        /// and fit the builder limit
        #[arg(
            long,
            default_value_t = GeocodeBuildConfig::default().street_level
        )]
        street_level: u8,
        /// Fallback cell level for rural areas; must be below --street-level
        /// and fit the builder limit
        #[arg(
            long,
            default_value_t = GeocodeBuildConfig::default().coarse_level
        )]
        coarse_level: u8,
        /// S2 cell level for admin boundaries; the builder enforces its work limit
        #[arg(
            long,
            default_value_t = GeocodeBuildConfig::default().admin_level
        )]
        admin_level: u8,
        /// Douglas-Peucker cap for local admin polygons; levels 5, 4, 3, and 2
        /// retain up to 2x, 4x, 8x, and 16x this value
        #[arg(
            long,
            default_value_t = GeocodeBuildConfig::default().max_admin_vertices
        )]
        max_admin_vertices: u16,
        /// Fine-level max search distance in meters
        #[arg(
            long,
            default_value_t = GeocodeBuildConfig::default().fine_search_radius_m
        )]
        search_radius: f32,
        /// Coarse-level max search distance in meters
        #[arg(
            long,
            default_value_t = GeocodeBuildConfig::default().coarse_search_radius_m
        )]
        coarse_search_radius: f32,
        #[command(flatten)]
        io: DirectIoArg,
        /// Replace an existing index (or empty directory) at --output-dir
        /// after the new build succeeds. Without it an existing --output-dir
        /// is refused before any work is done.
        #[arg(short = 'O', long)]
        overwrite: bool,
        /// Accept input without blob-level indexdata (slower fallback path).
        #[arg(long)]
        allow_unindexed: bool,
        /// Legacy shorthand for both --overwrite and --allow-unindexed; it
        /// warns that it also grants overwrite. Spell the two out instead.
        #[arg(long)]
        force: bool,
        #[command(flatten)]
        tmp: TmpDirArg,
    },
}

#[derive(Subcommand)]
enum InspectCommand {
    /// Count tag key=value frequencies
    Tags {
        /// Input PBF file
        file: PathBuf,
        /// Only show tags with at least this many occurrences
        #[arg(long, default_value = "1")]
        min_count: u64,
        /// Only show tags with at most this many occurrences
        #[arg(short = 'M', long)]
        max_count: Option<u64>,
        /// Sort order: count-desc (default), count-asc, name-asc, name-desc
        #[arg(long, default_value = "count-desc")]
        sort: TagSortArg,
        /// Read tag expressions from file (one per line, # comments)
        #[arg(short = 'e', long = "expressions")]
        expressions_file: Option<PathBuf>,
        /// Tag filter expressions (e.g. "highway", "amenity", "w/building=yes")
        expressions: Vec<String>,
        /// Filter by element type: node, way, or relation
        #[arg(short = 't', long = "type", value_parser = parse_single_type)]
        type_filter: Option<TypeFilter>,
        #[arg(
            short = 'j',
            long = "jobs",
            value_name = "N",
            default_value = "0",
            value_parser = parse_jobs,
            help = JOBS_HELP
        )]
        jobs: pbfhogg::Parallelism,
        #[command(flatten)]
        io: DirectIoArg,
        #[command(flatten)]
        force: ForceArg,
    },
}

/// `getid`'s three independent booleans: adjacent `bool`s that transpose
/// silently at a call site, so they are written by field name.
#[derive(Clone, Copy)]
struct GetidFlags {
    add_referenced: bool,
    remove_tags: bool,
    verbose_ids: bool,
}

/// `check`'s selectors and reporting booleans.
#[derive(Clone, Copy)]
struct CheckFlags {
    ids: bool,
    refs: bool,
    index: bool,
    check_relations: bool,
    show_ids: bool,
    full: bool,
    json: bool,
    quiet: bool,
}

/// `inspect`'s section selectors. `blocks` and `get` stay out: they carry
/// values rather than being part of the interchangeable-boolean run.
#[derive(Clone, Copy)]
struct InspectFlags {
    indexed: bool,
    nodes: bool,
    id_ranges: bool,
    locations: bool,
    anomalies: bool,
    extended: bool,
    json: bool,
    force: bool,
    distrust_metadata: bool,
    /// `-q, --quiet`: `--indexed` prints no verdict line.
    quiet: bool,
}

/// `diff`'s output-shape booleans.
#[derive(Clone, Copy)]
struct DiffFlags {
    suppress_common: bool,
    verbose: bool,
    osmium_summary: bool,
    quiet: bool,
}

/// The geocode index's S2 levels and search radii. The three `u8` levels
/// were adjacent positional parameters, as were the two `f32` radii - the
/// same silent-transposition hazard as the boolean runs, and harder to
/// notice because a wrong level still builds a valid-looking index.
#[derive(Clone, Copy)]
struct GeocodeLevels {
    street_level: u8,
    coarse_level: u8,
    admin_level: u8,
    max_admin_vertices: u16,
    search_radius: f32,
    coarse_search_radius: f32,
}

// Deliberately sniff only four bytes, then use extensions with unknown = PBF.
// Gzip magic means OSC; no BOM/whitespace skipping or format refusal is added.
// Stricter sniffing would break accepted inputs and the extension fallback contract.
fn detect_input_kind(path: &std::path::Path) -> InputKind {
    // Content sniffing: read first bytes to distinguish PBF from OSC
    if let Ok(kind) = sniff_input_kind(path) {
        return kind;
    }
    // Fall back to extension
    let name = path.to_string_lossy();
    if name.ends_with(".osc") || name.ends_with(".osc.gz") || name.ends_with(".osc.bz2") {
        InputKind::Osc
    } else {
        InputKind::Pbf
    }
}

/// `File::open` of an input this front end reads itself, naming `path` in a
/// failure (the library's opens do the same): a bare "No such file or
/// directory" does not say which input it was. The `io::ErrorKind` is kept.
#[allow(clippy::disallowed_methods)]
fn open_input(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    std::fs::File::open(path).map_err(|e| {
        std::io::Error::new(e.kind(), format!("failed to open {}: {e}", path.display()))
    })
}

fn sniff_input_kind(path: &std::path::Path) -> std::io::Result<InputKind> {
    use std::io::Read;
    let mut file = open_input(path)?;
    let mut buf = [0u8; 4];
    match file.read_exact(&mut buf) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
            return Err(std::io::Error::other("too small"));
        }
        Err(e) => return Err(e),
    }
    // Gzip magic (1f 8b) - PBF is never gzip-wrapped, so this is OSC (.osc.gz)
    if buf[0] == 0x1f && buf[1] == 0x8b {
        return Ok(InputKind::Osc);
    }
    // XML starts with '<' - this is OSC (.osc)
    if buf[0] == b'<' {
        return Ok(InputKind::Osc);
    }
    // PBF: first 4 bytes are big-endian u32 blob header size (typically 13-50)
    let size = u32::from_be_bytes(buf);
    if size > 0 && size < 1000 {
        return Ok(InputKind::Pbf);
    }
    Err(std::io::Error::other("ambiguous"))
}

/// Combine CLI positional expressions with expressions read from a file.
/// CLI expressions come first, then file expressions (additive, matching osmium).
fn combine_expressions(
    file: Option<&std::path::Path>,
    cli_args: &[String],
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let mut all = cli_args.to_vec();
    if let Some(path) = file {
        let from_file = pbfhogg::tag_expr::read_expressions_file(path)?;
        all.extend(from_file);
    }
    Ok(all)
}

/// The advice after an indexdata refusal: names only the switch this command
/// line could have used. A mode that needs indexdata whatever `--force` says
/// (`add-locations-to-ways --inject-prepass` or `--index-type external`,
/// `apply-changes --locations-on-ways`) gets none, since `--force` is refused
/// there. `build-geocode-index` takes `--allow-unindexed`. Every other command
/// that offers `--force` gets it.
fn indexdata_advice(command: &Command, leaf: &clap::ArgMatches) -> Option<&'static str> {
    match command {
        Command::BuildGeocodeIndex { .. } => {
            Some("\n\nOr pass --allow-unindexed to proceed anyway.")
        }
        Command::AddLocationsToWays {
            inject_prepass,
            index_type,
            ..
        } if *inject_prepass || index_type.resolve().ok() == Some(IndexType::External) => None,
        Command::ApplyChanges {
            locations_on_ways: true,
            ..
        } => None,
        _ => leaf
            .try_contains_id("force")
            .unwrap_or(false)
            .then_some("\n\nOr pass --force to proceed anyway."),
    }
}

/// The matches of the subcommand actually invoked.
fn leaf_matches(matches: &clap::ArgMatches) -> &clap::ArgMatches {
    let mut leaf = matches;
    while let Some((_, sub)) = leaf.subcommand() {
        leaf = sub;
    }
    leaf
}

/// Whether the invoked subcommand defines the boolean switch `id` and it was
/// left off: only then is naming it advice the operator can act on.
fn switch_on_offer(leaf: &clap::ArgMatches, id: &str) -> bool {
    leaf.try_get_one::<bool>(id)
        .ok()
        .flatten()
        .is_some_and(|on| !*on)
}

/// What this command line can be advised to do after a typed refusal; see
/// [`exit_code::Advice`].
fn advice(command: &Command, matches: &clap::ArgMatches) -> exit_code::Advice {
    let leaf = leaf_matches(matches);
    let element_cap = if leaf.try_contains_id("elements_per_blob").unwrap_or(false) {
        Some("--elements-per-blob")
    } else if leaf.try_contains_id("block_cap").unwrap_or(false) {
        Some("--block-cap")
    } else {
        None
    };
    exit_code::Advice {
        indexdata: indexdata_advice(command, leaf),
        element_cap,
        allow_version_regression: switch_on_offer(leaf, "allow_version_regression"),
        ignore_missing_nodes: switch_on_offer(leaf, "ignore_missing_nodes"),
    }
}

/// The whole `pbfhogg` process: parse the command line, run the command and
/// return the exit status. A process entry point, not a repeatable call: it
/// takes SIGINT and SIGTERM, reads the process arguments and environment and
/// sets up process-wide facilities.
#[allow(clippy::too_many_lines)]
pub fn run() -> process::ExitCode {
    // First, before any thread exists: SIGINT and SIGTERM become a
    // cancellation request (see `signals`). Every thread started from here on
    // inherits the blocked mask.
    let cancel = pbfhogg::host::Cancel::new();
    if let Err(e) = signals::install(cancel.clone()) {
        host::cli_warning(
            "cli.signals",
            format_args!(
                "cannot take SIGINT and SIGTERM ({e}); an interrupted run will not clean up \
                 its staging and scratch files"
            ),
        );
    }
    #[cfg(feature = "test-hooks")]
    signals::test_hooks::arm(&cancel);
    #[cfg(feature = "test-hooks")]
    signals::test_hooks::arm_input_replacement(&cancel);
    host::init_marker_fifo();
    let host = host::cli_host(cancel);
    let _guard = hotpath::HotpathGuardBuilder::new("pbfhogg::main")
        .percentiles(&[50.0, 95.0, 99.0])
        .functions_limit(0)
        .build();

    // `Cli::parse()` ends a parse error - and `--help` / `--version` - with
    // `clap::Error::exit`, which calls `process::exit` and so skips every
    // destructor, including the hotpath guard built two lines up. Use
    // `try_parse` and return instead so the guard is dropped normally.
    // `Error::print` writes to the same stream clap would have used
    // (stdout for help and version, stderr for a usage error) and
    // `Error::exit_code` is clap's own code for the case, so neither the
    // text nor the status a caller sees changes.
    //
    // The tree parsed is `cli_command()`, the derived one plus the retired
    // short flags; the matches are kept so a validation step can ask whether
    // a defaulted flag was given explicitly.
    let parsed = cli_command().try_get_matches().and_then(|matches| {
        <Cli as clap::FromArgMatches>::from_arg_matches(&matches).map(|cli| (cli, matches))
    });
    let (cli, matches) = match parsed {
        Ok(parsed) => parsed,
        Err(err) => return clap_exit(&err),
    };

    // From here on `--quiet` filters every diagnostic. The warnings printed
    // above (a signal or marker FIFO setup failure) are warnings, which it
    // keeps anyway.
    let quiet = cli.quiet;
    host::set_quiet(quiet);
    let advice = advice(&cli.command, &matches);
    let mut outcome = Outcome::Positive;
    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        // Every flag rule clap cannot express, before anything else looks at
        // the filesystem. Only tags-filter's autodetected input kind needs a
        // read (the four-byte sniff it always did), so its OSC-mode refusals
        // ride that read.
        let tags_filter_kind = validate_command(&cli.command, &matches, quiet)?;
        // The command line's inputs, outputs and scratch location, stated
        // once; the dispatch below moves the command's fields.
        let paths = CommandPaths::of(&cli.command, quiet);
        paths.refuse_aliases()?;
        let mut leftovers = LeftoverScanner::default();
        paths.scan_leftovers(&mut leftovers, &host);
        match cli.command {
            Command::Cat {
                files,
                output,
                clean,
                type_filter,
                dedupe,
                compression,
                io,
                io_uring,
                force,
                header,
            } => {
                let overrides = header.overrides();
                let write = WriteOptions {
                    compression: compression.compression,
                    backend: OutputBackend::from_flags(io.direct_io, io_uring),
                    direct_reads: io.direct_io,
                    force,
                };
                if dedupe {
                    run_merge_pbf(&files, output.open(&host)?, &write, &overrides, &host)
                } else {
                    run_cat(
                        &files,
                        output.open(&host)?,
                        type_filter,
                        &clean_attrs(&clean),
                        &write,
                        &overrides,
                        &host,
                    )
                }
            }
            Command::Sort {
                file,
                output,
                compression,
                io,
                uring,
                force,
                header,
            } => {
                let overrides = header.overrides();
                let write = WriteOptions {
                    compression: compression.compression,
                    backend: OutputBackend::from_flags(io.direct_io, uring.io_uring),
                    direct_reads: io.direct_io,
                    force: force.force,
                };
                run_sort(&file, output.open(&host)?, &write, &overrides, &host)
            }
            Command::Repack {
                file,
                output,
                elements_per_blob,
                compression,
                io,
                uring,
                force,
                header,
            } => {
                let overrides = header.overrides();
                let write = WriteOptions {
                    compression: compression.compression,
                    backend: OutputBackend::from_flags(io.direct_io, uring.io_uring),
                    direct_reads: io.direct_io,
                    force: force.force,
                };
                run_repack(
                    &file,
                    output.open(&host)?,
                    elements_per_blob,
                    &write,
                    &overrides,
                    &host,
                )
            }
            Command::Degrade {
                file,
                output,
                unsort,
                unsort_intra,
                strip_locations,
                strip_indexdata,
                strip_tagdata,
                corrupt_tagdata,
                corrupt_indexdata,
                corrupt_indexdata_kind,
                strip_bbox,
                drop_ids,
                block_cap,
                force,
                compression,
                io,
                uring,
                header,
            } => {
                let overrides = header.overrides();
                let write = WriteOptions {
                    compression: compression.compression,
                    backend: OutputBackend::from_flags(io.direct_io, uring.io_uring),
                    direct_reads: io.direct_io,
                    force,
                };
                // Six adjacent `bool`s used to be six positional arguments
                // here; the library's own `DegradeFlags` already names them,
                // so build it where the flags are read.
                let flags = pbfhogg::degrade::DegradeFlags {
                    unsort,
                    unsort_intra,
                    strip_locations,
                    strip_indexdata,
                    strip_tagdata,
                    corrupt_tagdata,
                    corrupt_indexdata,
                    corrupt_indexdata_kind,
                    strip_bbox,
                    drop_ids,
                };
                run_degrade(
                    &file,
                    output.open(&host)?,
                    flags,
                    block_cap,
                    &write,
                    &overrides,
                    &host,
                )
            }
            Command::Renumber {
                file,
                output,
                start_id,
                compression,
                io,
                header,
            } => {
                let overrides = header.overrides();
                let write = WriteOptions {
                    compression: compression.compression,
                    backend: OutputBackend::from_flags(io.direct_io, false),
                    direct_reads: io.direct_io,
                    force: false,
                };
                run_renumber(
                    &file,
                    output.open(&host)?,
                    start_id,
                    &write,
                    &overrides,
                    &host,
                )
            }
            Command::TagsFilter {
                file,
                output,
                input_kind: _,
                omit_referenced,
                invert_match,
                remove_tags,
                expressions_file: _,
                expressions: _,
                compression,
                force,
                distrust,
                io,
                jobs,
                header,
            } => {
                // `validate_command` read and checked the expressions before
                // the input was opened: a malformed expression, or an
                // expressions file with none in it, is refused first.
                let (kind, expressions) = tags_filter_kind.unwrap_or((InputKind::Pbf, Vec::new()));
                match kind {
                    InputKind::Osc => {
                        run_tags_filter_osc(&file, output.open(&host)?, &expressions, &host)
                    }
                    InputKind::Pbf => {
                        let write = WriteOptions {
                            compression: compression.compression,
                            backend: OutputBackend::from_flags(io.direct_io, false),
                            direct_reads: io.direct_io,
                            force,
                        };
                        let opts = pbfhogg::tags_filter::TagsFilterOptions {
                            expression_strs: &expressions,
                            omit_referenced,
                            invert: invert_match,
                            remove_tags,
                            write,
                            parallelism: jobs,
                            distrust_metadata: distrust.distrust_metadata,
                        };
                        opts.validate()?;
                        run_tags_filter(
                            &file,
                            output.open(&host)?,
                            &opts,
                            &header.overrides(),
                            &host,
                        )
                    }
                }
            }
            Command::Export {
                file,
                output,
                overwrite,
                format,
                type_filter,
                expressions_file,
                expressions,
                properties,
                bbox,
                metadata,
            } => {
                // The options (expressions included) are built before the
                // output is reserved, so a malformed expression is refused
                // first.
                let options = export_options(
                    format,
                    type_filter,
                    expressions_file.as_deref(),
                    &expressions,
                    properties,
                    bbox,
                    metadata,
                )?;
                run_export(
                    &file,
                    output
                        .as_deref()
                        .map(|path| {
                            pbfhogg::Output::new(path, overwrite.intent(), &host.diagnostics)
                        })
                        .transpose()?,
                    &options,
                    &host,
                )
            }
            Command::Diff {
                old,
                new,
                suppress_common,
                verbose,
                osmium_summary,
                output,
                overwrite,
                type_filter,
                jobs,
                format,
                increment_version,
                update_timestamp,
                // Placed by `paths`.
                tmp: _,
                io,
            } => match format {
                DiffFormat::Osc => {
                    // `validate_diff` refused a missing -o.
                    let output = output.ok_or("--output is required with --format osc")?;
                    // The output is checked before the scratch directory is
                    // made, so a missing output directory reports the output.
                    let output_handle =
                        pbfhogg::Output::new(&output, overwrite.intent(), &host.diagnostics)?;
                    let scratch = paths.create_scratch(&host)?;
                    accept_outcome(
                        &mut outcome,
                        run_derive_changes(
                            &old,
                            &new,
                            output_handle,
                            io.direct_io,
                            increment_version,
                            update_timestamp,
                            jobs,
                            &host,
                            scratch,
                        ),
                    )
                }
                DiffFormat::Text => {
                    let flags = DiffFlags {
                        suppress_common,
                        verbose,
                        osmium_summary,
                        quiet,
                    };
                    let output_handle = output
                        .as_deref()
                        .map(|path| {
                            pbfhogg::Output::new(path, overwrite.intent(), &host.diagnostics)
                        })
                        .transpose()?;
                    // Only a text diff that may shard writes scratch
                    // (`diff_writes_scratch`); it gets its directory now,
                    // before any input is opened.
                    let scratch = paths.create_scratch_if_any(&host)?;
                    let output = output_handle;
                    accept_outcome(
                        &mut outcome,
                        run_diff(
                            &old,
                            &new,
                            flags,
                            output,
                            type_filter,
                            jobs,
                            io.direct_io,
                            &host,
                            scratch,
                        ),
                    )
                }
            },
            Command::Digest { file, io } => {
                let digest = pbfhogg::digest::digest(&file, io.direct_io, &host)?;
                let mut out = output::buffered();
                digest.write_report(&mut out)?;
                out.flush()?;
                Ok(())
            }
            Command::Getid {
                file,
                output,
                invert,
                add_referenced,
                remove_tags,
                verbose_ids,
                id_file,
                id_osm_file,
                default_type,
                ids,
                compression,
                force,
                io,
                header,
            } => {
                let overrides = header.overrides();
                let write = WriteOptions {
                    compression: compression.compression,
                    backend: OutputBackend::from_flags(io.direct_io, false),
                    direct_reads: io.direct_io,
                    force: force.force,
                };
                // Positional IDs are parsed before the output is reserved; the
                // ID files are read after, so an existing output is refused
                // before a full pass over an -I PBF.
                let sources = IdSources::parse("getid", &ids, id_file, id_osm_file, default_type)?;
                let output = output.open(&host)?;
                // getid opens its input more than once, so it is pinned, and
                // before any ID file is read: every input of the run is
                // pinned before any is read (the -I PBF in `resolve`), and a
                // FIFO or missing input is refused before a full -I pass. The
                // same path given as the input and as -I is two inputs, each
                // read from one file throughout.
                let input = pbfhogg::read::input::PinnedInput::pin(&file, &host)?;
                if invert {
                    run_removeid(&input, output, sources, &write, &overrides, &host)
                } else {
                    let flags = GetidFlags {
                        add_referenced,
                        remove_tags,
                        verbose_ids,
                    };
                    run_getid(&input, output, flags, sources, &write, &overrides, &host)
                }
            }
            Command::Getparents {
                file,
                output,
                add_self,
                full_scan_min_blobs,
                id_file,
                id_osm_file,
                default_type,
                ids,
                compression,
                io,
                header,
            } => {
                let overrides = header.overrides();
                let write = WriteOptions {
                    compression: compression.compression,
                    backend: OutputBackend::from_flags(io.direct_io, false),
                    direct_reads: io.direct_io,
                    force: false,
                };
                let sources =
                    IdSources::parse("getparents", &ids, id_file, id_osm_file, default_type)?;
                let output = output.open(&host)?;
                // Pinned before any ID file is read, as getid's input: every
                // input of the run is pinned before any is read (the -I PBF
                // in `resolve`), and a FIFO or missing input is refused
                // before a full -I pass. The same path given as the input
                // and as -I is two inputs, each read from one file
                // throughout.
                let input = pbfhogg::read::input::PinnedInput::pin(&file, &host)?;
                run_getparents(
                    &input,
                    output,
                    add_self,
                    sources,
                    &write,
                    &overrides,
                    full_scan_min_blobs,
                    &host,
                )
            }
            Command::Extract {
                file,
                output,
                overwrite,
                bbox,
                polygon,
                config,
                directory,
                simple,
                smart,
                set_bounds,
                clean,
                compression,
                force,
                io,
                header,
            } => {
                let overrides = header.overrides();
                let write = WriteOptions {
                    compression: compression.compression,
                    backend: OutputBackend::from_flags(io.direct_io, false),
                    direct_reads: io.direct_io,
                    force,
                };
                let overwrite = pbfhogg::Overwrite::from_flag(overwrite);
                if let Some(config_path) = config.as_deref() {
                    run_extract_config(
                        &file,
                        config_path,
                        directory.as_deref(),
                        overwrite,
                        extract_strategy(simple, smart),
                        set_bounds,
                        &clean_attrs(&clean),
                        &write,
                        &overrides,
                        &host,
                        &mut leftovers,
                    )
                } else if let Some(output) = output.as_ref() {
                    // The region (a polygon file is read here) is parsed
                    // before the output is reserved.
                    let region = extract_region(bbox, polygon.as_deref())?;
                    run_extract(
                        &file,
                        pbfhogg::Output::new(output, overwrite, &host.diagnostics)?,
                        &region,
                        extract_strategy(simple, smart),
                        set_bounds,
                        &clean_attrs(&clean),
                        &write,
                        &overrides,
                        &host,
                    )
                } else {
                    Err("--output is required without --config".into())
                }
            }
            Command::AddLocationsToWays {
                file,
                output,
                keep_untagged_nodes,
                index_type,
                inject_prepass,
                ignore_missing_nodes,
                compression,
                force,
                io,
                header,
                // Placed by `paths`.
                tmp: _,
            } => {
                // `validate_command` refused the hidden `dense` value.
                let index_type = index_type.resolve()?;
                let overrides = header.overrides();
                let write = WriteOptions {
                    compression: compression.compression,
                    backend: OutputBackend::from_flags(io.direct_io, false),
                    direct_reads: io.direct_io,
                    force,
                };
                let options = pbfhogg::altw::AltwOptions {
                    keep_untagged_nodes,
                    write,
                    index_type,
                    inject_prepass,
                    ignore_missing_nodes,
                };
                // The output is checked before the scratch directory is made,
                // so a missing output directory reports the output.
                let output = output.open(&host)?;
                let scratch = paths.create_scratch(&host)?;
                run_add_locations_to_ways(&file, output, &options, &overrides, &host, scratch)
            }
            Command::TimeFilter {
                file,
                output,
                timestamp,
                compression,
                io,
                header,
            } => {
                let overrides = header.overrides();
                let write = WriteOptions {
                    compression: compression.compression,
                    backend: OutputBackend::from_flags(io.direct_io, false),
                    direct_reads: io.direct_io,
                    force: false,
                };
                run_time_filter(
                    &file,
                    output.open(&host)?,
                    timestamp,
                    &write,
                    &overrides,
                    &host,
                )
            }
            Command::Inspect {
                subcommand,
                file,
                indexed,
                nodes,
                jobs,
                blocks,
                id_ranges,
                locations,
                anomalies,
                extended,
                show,
                get,
                json,
                io,
                distrust,
                force,
            } => {
                if let Some(InspectCommand::Tags {
                    file: tags_file,
                    min_count,
                    max_count,
                    sort,
                    expressions_file,
                    expressions,
                    type_filter,
                    jobs: tags_jobs,
                    io: tags_io,
                    force: tags_force,
                }) = subcommand
                {
                    run_tags_count(
                        &tags_file,
                        min_count,
                        max_count,
                        sort.into(),
                        expressions_file.as_deref(),
                        &expressions,
                        type_filter,
                        tags_jobs,
                        tags_io.direct_io,
                        tags_force.force,
                        &host,
                    )
                } else {
                    #[expect(
                        clippy::expect_used,
                        reason = "clap requires the file unless a subcommand is given (required = true with subcommand_negates_reqs)"
                    )]
                    let file = file.expect("clap requires a file without an inspect subcommand");
                    if let Some(show_id) = show {
                        return accept_outcome(
                            &mut outcome,
                            run_show_element(&file, show_id, io.direct_io, &host),
                        );
                    }
                    accept_outcome(
                        &mut outcome,
                        run_inspect(
                            &file,
                            InspectFlags {
                                indexed,
                                nodes,
                                id_ranges,
                                locations,
                                anomalies,
                                extended,
                                json,
                                force,
                                distrust_metadata: distrust.distrust_metadata,
                                quiet,
                            },
                            jobs,
                            blocks,
                            get.as_deref(),
                            io.direct_io,
                            &host,
                        ),
                    )
                }
            }
            Command::ApplyChanges {
                base,
                changes,
                output,
                compression,
                force,
                io,
                uring,
                locations_on_ways,
                ignore_missing_nodes,
                allow_version_regression,
                jobs,
                header,
            } => {
                let overrides = header.overrides();
                let write = WriteOptions {
                    compression: compression.compression,
                    backend: OutputBackend::from_flags(io.direct_io, uring.io_uring),
                    direct_reads: io.direct_io,
                    force,
                };
                let opts = pbfhogg::apply_changes::MergeOptions {
                    write,
                    locations_on_ways,
                    ignore_missing_nodes,
                    parallelism: jobs,
                    allow_version_regression,
                };
                opts.validate()?;
                run_apply_changes(
                    &base,
                    &changes,
                    output.open(&host)?,
                    &opts,
                    &overrides,
                    &host,
                )
            }
            Command::MergeChanges {
                changes,
                output,
                simplify,
                allow_version_regression,
                jobs,
            } => run_merge_changes(
                &changes,
                output.open(&host)?,
                simplify,
                jobs,
                allow_version_regression,
                &host,
            ),
            Command::Check {
                file,
                ids,
                refs,
                index,
                check_relations,
                show_ids,
                full,
                type_filter,
                max_errors,
                json,
                io,
            } => accept_outcome(
                &mut outcome,
                run_check(
                    &file,
                    CheckFlags {
                        ids,
                        refs,
                        index,
                        check_relations,
                        show_ids,
                        full,
                        json,
                        quiet,
                    },
                    type_filter,
                    max_errors,
                    io.direct_io,
                    &host,
                ),
            ),
            Command::BuildGeocodeIndex {
                file,
                output_dir,
                street_level,
                coarse_level,
                admin_level,
                max_admin_vertices,
                search_radius,
                coarse_search_radius,
                io,
                overwrite,
                allow_unindexed,
                force,
                // Placed by `paths`.
                tmp: _,
            } => {
                // The one --force that is not the indexdata rule: it predates
                // --overwrite and --allow-unindexed and still grants both, so
                // say so every time rather than let it replace an index
                // silently.
                if force {
                    host::cli_warning(
                        "geocode.force_grants_overwrite",
                        format_args!(
                            "build-geocode-index --force also grants --overwrite: an existing \
                             index at --output-dir is replaced. Pass --overwrite and/or \
                             --allow-unindexed instead to say which you mean."
                        ),
                    );
                }
                run_build_geocode_index(
                    &file,
                    &output_dir,
                    GeocodeLevels {
                        street_level,
                        coarse_level,
                        admin_level,
                        max_admin_vertices,
                        search_radius,
                        coarse_search_radius,
                    },
                    io.direct_io,
                    overwrite || force,
                    allow_unindexed || force,
                    &paths,
                    &host,
                )
            }
            Command::BenchRead { file, mode } => run_bench_read(&file, mode, &host),
            Command::BenchWrite {
                file,
                compression,
                writer,
            } => run_bench_write(&file, compression, writer, &host),
            Command::BenchMerge {
                base,
                changes,
                output,
                compression,
                io_mode,
            } => run_bench_merge(
                &base,
                &changes,
                output.open(&host)?,
                compression,
                io_mode,
                &host,
            ),
        }
    })();

    // Emit the `writer_*` sidecar counters exactly once, here rather than
    // from `PbfWriter::flush`. The counters are a process-global static, so
    // flush-time emission published the same running totals once per writer -
    // once per region for `multi-extract` - which reads as a per-region
    // breakdown and is not one. This is the single point every command path
    // passes through, success or failure, and it runs before the hotpath
    // guard drops.
    pbfhogg::write::emit_writer_counters();
    pbfhogg::read::emit_read_counters();
    pbfhogg::write::report_writer_degradations(&host.diagnostics);
    #[cfg(feature = "test-hooks")]
    signals::test_hooks::write_count(&host.cancel);
    #[cfg(feature = "test-hooks")]
    signals::test_hooks::write_replacement_report(&host.cancel);

    // From here on the outcome is decided; a "stopping" notice the signal
    // thread has not printed yet would be stale.
    signals::finish();
    let signal = signals::first_signal(&host.cancel);
    match result {
        Ok(()) => {
            if let Some(signo) = signal {
                // The work and any publication had completed (every early
                // stop is an error, and publication refuses once the token
                // is tripped before its decision): the result stands.
                host::cli_note(
                    "cli.signal_after_completion",
                    format_args!(
                        "{} arrived after the run had completed; its result stands",
                        signals::name(signo)
                    ),
                );
            }
            process::ExitCode::from(outcome.code())
        }
        // Decided before the reader-gone override: an interrupted run whose
        // stdout reader also left must not report success.
        Err(e) if is_cancelled(e.as_ref()) => {
            output::cli_eprintln!(
                "Error: interrupted by {}: the run stopped before completing, removed its \
                 staging and scratch files and published nothing",
                signal.map_or("a cancellation request", signals::name)
            );
            process::ExitCode::from(signals::exit_status(signal))
        }
        // A refusal found by a command's validation step: printed by clap,
        // with clap's status, exactly like a refusal clap found itself.
        Err(e) if e.is::<UsageError>() => match e.downcast_ref::<UsageError>() {
            Some(UsageError(err)) => clap_exit(err),
            None => process::ExitCode::from(ExitStatus::Failed.code()),
        },
        Err(e) => {
            if output::reader_gone() {
                // Stdout's reader went away (`pbfhogg ... | head`) and the
                // command stopped writing. Nobody is left to read a report
                // and nothing failed, so end quietly with success - see
                // `output` for why this is keyed on the recorded event
                // rather than on the shape of `e`.
                process::ExitCode::SUCCESS
            } else {
                output::cli_eprintln!(
                    "Error: {}{}",
                    error_report(e.as_ref()),
                    output_hint(e.as_ref(), &advice).unwrap_or_default()
                );
                let status = if is_platform_unsupported(e.as_ref()) {
                    ExitStatus::PlatformUnsupported
                } else {
                    ExitStatus::Failed
                };
                process::ExitCode::from(status.code())
            }
        }
    }
    // `_guard` drops here, flushing the hotpath JSON report before the
    // returned `ExitCode` is converted to the process exit status.
}

#[allow(clippy::too_many_arguments)]
fn run_build_geocode_index(
    file: &std::path::Path,
    output_dir: &std::path::Path,
    levels: GeocodeLevels,
    direct_io: bool,
    overwrite: bool,
    allow_unindexed: bool,
    paths: &CommandPaths,
    host: &pbfhogg::host::Host,
) -> std::result::Result<(), Box<dyn std::error::Error>> {
    // The caller has already expanded the legacy `--force` into both
    // permissions. Refusals happen here, before any work.
    let output = pbfhogg::geocode_index::builder::index_output(
        output_dir,
        pbfhogg::Overwrite::from_flag(overwrite),
        &host.diagnostics,
    )?;
    // After the output, so an existing index directory is judged before the
    // run's scratch directory could be made inside it.
    let scratch = paths.create_scratch(host)?;
    // Exhaustive on purpose: a new `BuildConfig` field must be wired here
    // rather than silently taking its default. The clap defaults already
    // come from `BuildConfig::default()`.
    let config = GeocodeBuildConfig {
        input_path: file.to_path_buf(),
        allow_unindexed,
        direct_io,
        street_level: levels.street_level,
        coarse_level: levels.coarse_level,
        admin_level: levels.admin_level,
        max_admin_vertices: levels.max_admin_vertices,
        fine_search_radius_m: levels.search_radius,
        coarse_search_radius_m: levels.coarse_search_radius,
    };
    // The library reports the counts itself, ahead of its closing "Done" line.
    pbfhogg::geocode_index::builder::build_geocode_index(&config, output, host, scratch)?;
    Ok(())
}

#[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
fn run_check(
    path: &std::path::Path,
    flags: CheckFlags,
    type_filter: Option<TypeFilter>,
    max_errors: usize,
    direct_io: bool,
    host: &pbfhogg::host::Host,
) -> Result<Outcome, Box<dyn std::error::Error>> {
    let CheckFlags {
        ids,
        refs,
        index,
        check_relations,
        show_ids,
        full,
        json,
        quiet,
    } = flags;
    // Default: run the --ids + --refs pair when no selector is given.
    //
    // `--index` is a selector like the other two, but it is NOT part of the
    // default set: it decompresses the entire file to re-derive every blob's
    // metadata, so folding it into a bare `check` would silently multiply the
    // cost of the command everyone already runs. It does suppress the default,
    // so `check --index` runs only the metadata verification.
    let any_selector = ids || refs || index;
    let run_ids = ids || !any_selector;
    let run_refs = refs || !any_selector;
    let run_index = index;
    // Flags that apply to one check mode only were refused by
    // `validate_check`.

    let mut failed = false;
    let file_name = path
        .file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy();
    let check_options = pbfhogg::check::CheckOptions {
        max_errors,
        direct_io,
    };

    // With two or more checks selected (a bare `check` is ids then refs) the
    // run is several library calls on one input, each opening it afresh, so
    // the input is pinned here, before any of them, and each reads through
    // the pin: a file renamed over the input between two checks cannot have
    // one checked as one file and the next as the other. Alone, each check
    // decides for itself: `--ids` pins (it opens twice), `--index` and
    // `--refs` open once, so `--index` alone still reads a FIFO.
    let pin = if [run_ids, run_index, run_refs]
        .into_iter()
        .filter(|&run| run)
        .count()
        > 1
    {
        Some(pbfhogg::read::input::PinnedInput::pin(path, host)?)
    } else {
        None
    };
    let input: pbfhogg::read::input::InputSource<'_> = pin.as_ref().map_or(path.into(), Into::into);

    // Run checks and collect results
    let ids_report = if run_ids {
        use pbfhogg::check::verify_ids::VerifyIdsOptions;
        let opts = VerifyIdsOptions {
            check: check_options,
            full,
            type_filter,
        };
        let report = pbfhogg::check::verify_ids::verify_ids(input, &opts, host)?;
        if !report.passed {
            failed = true;
        }
        Some(report)
    } else {
        None
    };

    let index_report = if run_index {
        let report = pbfhogg::check::index::check_index(input, &check_options, host)?;
        if !report.passed() {
            failed = true;
        }
        Some(report)
    } else {
        None
    };

    let refs_result = if run_refs {
        let opts = pbfhogg::check::refs::CheckRefsOptions {
            check: check_options,
            check_relations,
            show_ids,
        };
        let result = pbfhogg::check::refs::check_refs(input, &opts, host)?;
        if !result.is_valid() {
            failed = true;
        }
        Some(result)
    } else {
        None
    };

    // Output. The verdict is decided above, before a byte is written, so a
    // reader that stops early (`check x | head -1`) must not cost the exit
    // status that carries it.
    let printed = (|| -> Result<(), Box<dyn std::error::Error>> {
        let mut out = output::buffered();
        if json {
            let mut combined = serde_json::Map::new();
            if let Some(ref report) = ids_report {
                combined.insert("ids".to_string(), report.to_json_value(&file_name));
            }
            if let Some(ref report) = index_report {
                combined.insert("index".to_string(), report.to_json_value(&file_name));
            }
            if let Some(ref result) = refs_result {
                let mut refs_obj = serde_json::json!({
                    "node_count": result.node_count,
                    "way_count": result.way_count,
                    "relation_count": result.relation_count,
                    "missing_node_refs": result.missing_node_refs,
                    "missing_way_refs": result.missing_way_refs,
                    "missing_node_members": result.missing_node_members,
                    "missing_relation_members": result.missing_relation_members,
                    "missing_relation_member_occurrences": result.missing_relation_member_occurrences,
                    "untrackable_node_ids": result.untrackable_ids[0],
                    "untrackable_way_ids": result.untrackable_ids[1],
                    "untrackable_relation_ids": result.untrackable_ids[2],
                    "passed": result.is_valid(),
                });
                if show_ids {
                    let details: Vec<_> = result.missing_refs.iter().map(|mref| {
                    serde_json::json!({
                        "missing": format!("{}{}", mref.missing_type, mref.missing_id),
                        "referenced_by": format!("{}{}", mref.referencing_type, mref.referencing_id),
                    })
                }).collect();
                    if let Some(m) = refs_obj.as_object_mut() {
                        m.insert(
                            "missing_details".to_string(),
                            serde_json::Value::Array(details),
                        );
                    }
                }
                combined.insert("refs".to_string(), refs_obj);
            }
            writeln!(
                out,
                "{}",
                serde_json::to_string_pretty(&serde_json::Value::Object(combined))?
            )?;
        } else if !quiet {
            if let Some(ref report) = ids_report {
                report.write_human(&mut out, &file_name)?;
            }
            if let Some(ref report) = index_report {
                if ids_report.is_some() {
                    writeln!(out, "\n---\n")?;
                }
                report.write_human(&mut out, &file_name)?;
                if report.total_violations > report.violations.len() as u64 {
                    host::cli_note(
                        "cli.check_index_capped",
                        format_args!("raise --max-errors to see every violation"),
                    );
                }
            }
            if let Some(ref result) = refs_result {
                if show_ids {
                    for mref in &result.missing_refs {
                        writeln!(
                            out,
                            "{}{} in {}{}",
                            mref.missing_type,
                            mref.missing_id,
                            mref.referencing_type,
                            mref.referencing_id,
                        )?;
                    }
                }
                if ids_report.is_some() || index_report.is_some() {
                    writeln!(out, "\n---\n")?;
                }
                writeln!(
                    out,
                    "Elements: {} nodes, {} ways, {} relations",
                    result.node_count, result.way_count, result.relation_count
                )?;
                if result.missing_node_refs > 0 {
                    writeln!(
                        out,
                        "Missing node refs in ways: {}",
                        result.missing_node_refs
                    )?;
                }
                if check_relations {
                    if result.missing_way_refs > 0 {
                        writeln!(
                            out,
                            "Missing way refs in relations: {}",
                            result.missing_way_refs
                        )?;
                    }
                    if result.missing_node_members > 0 {
                        writeln!(
                            out,
                            "Missing node members in relations: {}",
                            result.missing_node_members
                        )?;
                    }
                    if result.missing_relation_members > 0 {
                        if result.missing_relation_member_occurrences
                            > result.missing_relation_members
                        {
                            writeln!(
                                out,
                                "Missing relation members: {} ({} references)",
                                result.missing_relation_members,
                                result.missing_relation_member_occurrences,
                            )?;
                        } else {
                            writeln!(
                                out,
                                "Missing relation members: {}",
                                result.missing_relation_members
                            )?;
                        }
                    }
                }
                // An untrackable ID is not itself a missing reference, but every
                // reference to one is counted as missing above. Say so, or the
                // over-report reads as a genuinely dangling ref.
                if result.total_untrackable() > 0 {
                    writeln!(
                        out,
                        "Untrackable element IDs: {} nodes, {} ways, {} relations \
                     - references to these are counted as missing",
                        result.untrackable_ids[0],
                        result.untrackable_ids[1],
                        result.untrackable_ids[2],
                    )?;
                }
                if result.is_valid() {
                    writeln!(out, "Referential integrity: OK")?;
                } else {
                    writeln!(
                        out,
                        "Referential integrity: FAILED ({} missing references)",
                        result.total_missing()
                    )?;
                }
            }
        }
        out.flush()?;
        Ok(())
    })();
    output::tolerate_reader_gone(printed)?;

    Ok(if failed {
        Outcome::Negative
    } else {
        Outcome::Positive
    })
}

#[allow(clippy::too_many_arguments)]
fn run_tags_count(
    path: &std::path::Path,
    min_count: u64,
    max_count: Option<u64>,
    sort: TagCountSort,
    expressions_file: Option<&std::path::Path>,
    expressions: &[String],
    type_filter: Option<TypeFilter>,
    parallelism: pbfhogg::Parallelism,
    direct_io: bool,
    force: bool,
    host: &pbfhogg::host::Host,
) -> Result<(), Box<dyn std::error::Error>> {
    let all_expressions = combine_expressions(expressions_file, expressions)?;
    pbfhogg::tag_expr::validate_expressions(&all_expressions)
        .map_err(|e| usage(&["inspect", "tags"], format!("invalid expression: {e}")))?;
    let opts = pbfhogg::tags_count::TagCountOptions {
        min_count,
        max_count,
        sort,
        expressions: &all_expressions,
        type_filter,
        direct_io,
        force,
        parallelism,
    };
    let results = pbfhogg::tags_count::tags_count(path, &opts, host)?;

    let mut out = output::buffered();
    for entry in &results {
        writeln!(out, "{}\t{}\t{}", entry.count, entry.key, entry.value)?;
    }
    out.flush()?;

    writeln!(
        host.diagnostics.summary_writer("tags_count.summary"),
        "{} distinct tag values",
        results.len()
    )?;
    Ok(())
}

fn run_cat(
    files: &[PathBuf],
    output: pbfhogg::Output,
    type_filter: Option<TypeFilter>,
    clean: &CleanAttrs,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &pbfhogg::host::Host,
) -> Result<(), Box<dyn std::error::Error>> {
    let paths: Vec<&std::path::Path> = files.iter().map(AsRef::as_ref).collect();
    let stats = pbfhogg::cat::cat(&paths, output, type_filter, clean, write, overrides, host)?;
    stats.write_summary(&mut host.diagnostics.summary_writer("cat.summary"))?;
    Ok(())
}

fn run_sort(
    file: &std::path::Path,
    output: pbfhogg::Output,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &pbfhogg::host::Host,
) -> Result<(), Box<dyn std::error::Error>> {
    let stats = pbfhogg::sort::sort(file, output, write, overrides, host)?;
    stats.write_summary(&mut host.diagnostics.summary_writer("sort.summary"))?;
    host::note_unverified_claims(stats.passthrough_claimed);
    Ok(())
}

fn run_repack(
    file: &std::path::Path,
    output: pbfhogg::Output,
    elements_per_blob: std::num::NonZeroUsize,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &pbfhogg::host::Host,
) -> Result<(), Box<dyn std::error::Error>> {
    let stats = pbfhogg::repack::repack(file, output, elements_per_blob, write, overrides, host)?;
    stats.write_summary(&mut host.diagnostics.summary_writer("repack.summary"))?;
    Ok(())
}

fn run_degrade(
    file: &std::path::Path,
    output: pbfhogg::Output,
    flags: pbfhogg::degrade::DegradeFlags,
    block_cap: std::num::NonZeroUsize,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &pbfhogg::host::Host,
) -> Result<(), Box<dyn std::error::Error>> {
    let stats = pbfhogg::degrade::degrade(file, output, flags, block_cap, write, overrides, host)?;
    stats.write_summary(&mut host.diagnostics.summary_writer("degrade.summary"))?;
    Ok(())
}

fn run_renumber(
    file: &std::path::Path,
    output: pbfhogg::Output,
    start_id: StartIds,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &pbfhogg::host::Host,
) -> Result<(), Box<dyn std::error::Error>> {
    let opts = pbfhogg::renumber::RenumberOptions {
        start_node_id: start_id.node,
        start_way_id: start_id.way,
        start_relation_id: start_id.relation,
    };
    let stats = pbfhogg::renumber::renumber_external(file, output, &opts, write, overrides, host)?;
    stats.write_summary(&mut host.diagnostics.summary_writer("renumber.summary"))?;
    stats.report_warnings(&host.diagnostics);
    Ok(())
}

/// tags-filter's expressions: the positional ones, then the `-e` file's,
/// checked to parse and to be non-empty. clap already refused a command line
/// with neither; this catches an `-e` file that holds none.
fn filter_expressions(
    expressions_file: Option<&std::path::Path>,
    expressions: &[String],
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let all = combine_expressions(expressions_file, expressions)?;
    if all.is_empty() {
        return Err(usage(
            &["tags-filter"],
            "no filter expressions: the --expressions file holds none",
        ));
    }
    pbfhogg::tag_expr::validate_expressions(&all)
        .map_err(|e| usage(&["tags-filter"], format!("invalid expression: {e}")))?;
    Ok(all)
}

fn run_tags_filter(
    file: &std::path::Path,
    output: pbfhogg::Output,
    opts: &pbfhogg::tags_filter::TagsFilterOptions<'_>,
    overrides: &HeaderOverrides,
    host: &pbfhogg::host::Host,
) -> Result<(), Box<dyn std::error::Error>> {
    let stats = pbfhogg::tags_filter::tags_filter(file, output, opts, overrides, host)?;
    stats.write_summary(&mut host.diagnostics.summary_writer("tags_filter.summary"))?;
    Ok(())
}

fn run_tags_filter_osc(
    changes: &std::path::Path,
    output: pbfhogg::Output,
    expressions: &[String],
    host: &pbfhogg::host::Host,
) -> Result<(), Box<dyn std::error::Error>> {
    let stats = pbfhogg::tags_filter::osc::tags_filter_osc(changes, output, expressions, host)?;
    stats.write_summary(&mut host.diagnostics.summary_writer("tags_filter.osc.summary"))?;
    Ok(())
}

fn run_export_to<W: std::io::Write>(
    file: &std::path::Path,
    writer: W,
    options: &pbfhogg::export::ExportOptions,
    host: &pbfhogg::host::Host,
) -> Result<(), Box<dyn std::error::Error>> {
    let stats = pbfhogg::export::export(file, writer, options, host).map_err(export_error)?;
    stats.write_summary(&mut host.diagnostics.summary_writer("export_to.summary"))?;
    Ok(())
}

/// The library reports a missing header feature without naming any CLI
/// flag - a `Display` impl in a library has no idea which front end is
/// reading it. Naming `--type node` is this binary's job, so the advice
/// is attached here, where the flag actually exists.
fn export_error(err: pbfhogg::Error) -> Box<dyn std::error::Error> {
    if matches!(
        err.kind(),
        pbfhogg::ErrorKind::MissingFeature(f) if *f == pbfhogg::HeaderBlock::LOCATIONS_ON_WAYS
    ) {
        return Box::new(Hinted::new(err, "; use --type node to export nodes only"));
    }
    Box::new(err)
}

fn export_options(
    format: ExportFormatArg,
    type_filter: Option<pbfhogg::export::ExportTypes>,
    expressions_file: Option<&std::path::Path>,
    expressions: &[String],
    properties: Option<Vec<String>>,
    bbox: Option<pbfhogg::extract::Bbox>,
    metadata: bool,
) -> Result<pbfhogg::export::ExportOptions, Box<dyn std::error::Error>> {
    let expressions = combine_expressions(expressions_file, expressions)?;
    let format = match format {
        ExportFormatArg::Geojsonseq => pbfhogg::export::ExportFormat::GeoJsonSeq,
        ExportFormatArg::Geojson => pbfhogg::export::ExportFormat::GeoJson,
    };
    let types = type_filter.unwrap_or(pbfhogg::export::ExportTypes::All);
    // Only the expressions can fail here, and a malformed one is a usage
    // error like any other malformed flag value.
    pbfhogg::export::ExportOptions::new(format, types, &expressions, properties, bbox, metadata)
        .map_err(|e| usage(&["export"], format!("invalid expression: {e}")))
}

fn run_export(
    file: &std::path::Path,
    output: Option<pbfhogg::Output>,
    options: &pbfhogg::export::ExportOptions,
    host: &pbfhogg::host::Host,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(output) = output {
        // The input/output alias refusal already ran in
        // `validate_input_output_aliases`, against the destination.
        let mut buf = output.writer(&host.cancel)?;
        run_export_to(file, &mut buf, options, host)?;
        // The checked flush that lets the output be published.
        buf.finish()?;
        output.publish(&host.cancel)?;
    } else {
        // `export` flushes its writer when it finishes, so a failed final
        // flush surfaces as an error rather than vanishing in a drop.
        run_export_to(file, output::buffered(), options, host)?;
    }
    Ok(())
}

/// getid's and getparents' IDs: the positional ones already parsed (they
/// depend on `--default-type`, so they are parsed right after clap, before
/// any input is opened), and the ID files still to read.
struct IdSources {
    positional: pbfhogg::getid::ElementIds,
    id_file: Option<PathBuf>,
    id_osm_file: Option<PathBuf>,
    default_type: Option<ElementKind>,
}

impl IdSources {
    /// `command` names the subcommand for a usage error on a malformed ID.
    fn parse(
        command: &str,
        ids: &[String],
        id_file: Option<PathBuf>,
        id_osm_file: Option<PathBuf>,
        default_type: Option<DefaultTypeArg>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let default_type = default_type.map(ElementKind::from);
        Ok(Self {
            positional: pbfhogg::getid::parse_ids_with_default_type(ids, default_type)
                .map_err(|e| usage(&[command], e))?,
            id_file,
            id_osm_file,
            default_type,
        })
    }

    /// Read the ID files and merge everything. clap already refused a
    /// command line naming no ID source; this catches sources that hold no
    /// ID.
    ///
    /// The -I PBF is pinned before either ID file is read, so it is refused
    /// up front when it is not a regular file (a FIFO would
    /// block its open), and its one read goes through the pin.
    fn resolve(
        self,
        direct_io: bool,
        host: &pbfhogg::host::Host,
    ) -> Result<pbfhogg::getid::ElementIds, Box<dyn std::error::Error>> {
        let id_osm_pin = self
            .id_osm_file
            .as_deref()
            .map(|path| pbfhogg::read::input::PinnedInput::pin(path, host))
            .transpose()?;
        let mut id_set = self.positional;
        if let Some(path) = self.id_file.as_deref() {
            let file_ids =
                pbfhogg::getid::parse_ids_from_file_with_default_type(path, self.default_type)?;
            pbfhogg::getid::merge_id_sets(&mut id_set, &file_ids);
        }
        if let Some(pin) = id_osm_pin.as_ref() {
            let pbf_ids = pbfhogg::getid::parse_ids_from_pbf(pin, direct_io, host)?;
            pbfhogg::getid::merge_id_sets(&mut id_set, &pbf_ids);
        }
        if id_set.is_empty() {
            return Err("no IDs: the --id-file and --id-osm-file inputs hold none".into());
        }
        Ok(id_set)
    }
}

// `--verbose-ids` is a deliberately exhaustive diagnostic: no output cap,
// and `print_missing_ids` collects the missing IDs per kind before printing.
// With `-I` every input ID participates, so output and storage scale with
// the input. Both CLI references document this under getid; capping it
// would make the listing incomplete without saying which IDs were cut.
fn print_requested_ids(ids: &pbfhogg::getid::ElementIds) {
    output::cli_eprintln!("Requested IDs:");
    if ids.node_ids.has_any() {
        output::cli_eprint!("  nodes:");
        for id in ids.node_ids.iter() {
            output::cli_eprint!(" {id}");
        }
        output::cli_eprintln!();
    }
    if ids.way_ids.has_any() {
        output::cli_eprint!("  ways:");
        for id in ids.way_ids.iter() {
            output::cli_eprint!(" {id}");
        }
        output::cli_eprintln!();
    }
    if ids.relation_ids.has_any() {
        output::cli_eprint!("  relations:");
        for id in ids.relation_ids.iter() {
            output::cli_eprint!(" {id}");
        }
        output::cli_eprintln!();
    }
}

/// Report the requested IDs that never made it into the output.
///
/// `found` comes from `getid`'s own write path. This used to decode the
/// finished output PBF a second time, which roughly doubled the
/// command's runtime at planet scale for what is only a diagnostic.
fn print_missing_ids(requested: &pbfhogg::getid::ElementIds, found: &pbfhogg::getid::ElementIds) {
    let missing_nodes: Vec<_> = requested
        .node_ids
        .iter()
        .filter(|&id| !found.node_ids.get(id))
        .collect();
    let missing_ways: Vec<_> = requested
        .way_ids
        .iter()
        .filter(|&id| !found.way_ids.get(id))
        .collect();
    let missing_rels: Vec<_> = requested
        .relation_ids
        .iter()
        .filter(|&id| !found.relation_ids.get(id))
        .collect();

    let total_missing = missing_nodes.len() + missing_ways.len() + missing_rels.len();
    if total_missing == 0 {
        output::cli_eprintln!("Found all requested objects.");
    } else {
        output::cli_eprintln!("Did not find {total_missing} object(s):");
        for id in &missing_nodes {
            output::cli_eprintln!("  n{id}");
        }
        for id in &missing_ways {
            output::cli_eprintln!("  w{id}");
        }
        for id in &missing_rels {
            output::cli_eprintln!("  r{id}");
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_diff(
    old: &std::path::Path,
    new: &std::path::Path,
    flags: DiffFlags,
    output: Option<pbfhogg::Output>,
    type_filter: Option<TypeFilter>,
    parallelism: pbfhogg::Parallelism,
    direct_io: bool,
    host: &pbfhogg::host::Host,
    scratch: Option<pbfhogg::scratch::RunScratch>,
) -> Result<Outcome, Box<dyn std::error::Error>> {
    let DiffFlags {
        suppress_common,
        verbose,
        osmium_summary,
        quiet,
    } = flags;

    let options = pbfhogg::diff::DiffOptions {
        suppress_common,
        verbose,
        type_filter,
        parallelism,
    };
    let stats = if quiet {
        // The answer is the exit code: count, formatting nothing.
        // `validate_diff` refused every flag that shapes the text, and the
        // dispatch made no scratch directory.
        pbfhogg::diff::diff_stats(old, new, type_filter, parallelism, direct_io, host)?
    } else if let Some(output) = output {
        let mut out = output.writer(&host.cancel)?;
        let stats = pbfhogg::diff::diff(old, new, &mut out, &options, direct_io, host, scratch)?;
        out.finish()?;
        // A diff that found differences is a completed answer, not a
        // failure: its output is published like any other.
        output.publish(&host.cancel)?;
        stats
    } else {
        // A reader that leaves early stops the diff here, before its verdict
        // exists, so the run ends with success rather than with the
        // differences status `run` cannot know.
        let mut out = output::buffered();
        let stats = pbfhogg::diff::diff(old, new, &mut out, &options, direct_io, host, scratch)?;
        out.flush()?;
        stats
    };
    if !quiet {
        if osmium_summary {
            stats.write_osmium_summary(
                &mut host.diagnostics.summary_writer("diff.osmium_summary"),
            )?;
        } else {
            stats.write_summary(&mut host.diagnostics.summary_writer("diff.summary"))?;
        }
    }
    Ok(if stats.has_differences() {
        Outcome::Negative
    } else {
        Outcome::Positive
    })
}

#[allow(clippy::too_many_arguments)]
fn run_derive_changes(
    old: &std::path::Path,
    new: &std::path::Path,
    output: pbfhogg::Output,
    direct_io: bool,
    increment_version: bool,
    update_timestamp: bool,
    parallelism: pbfhogg::Parallelism,
    host: &pbfhogg::host::Host,
    scratch: pbfhogg::scratch::RunScratch,
) -> Result<Outcome, Box<dyn std::error::Error>> {
    let timestamp = if update_timestamp {
        Some(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs(),
        )
    } else {
        None
    };
    let stats = pbfhogg::diff::derive::derive_changes(
        old,
        new,
        output,
        direct_io,
        increment_version,
        timestamp,
        parallelism,
        host,
        scratch,
    )?;
    stats.write_summary(&mut host.diagnostics.summary_writer("derive.summary"))?;
    // The same verdict as a text diff: an OSC diff that holds changes is a
    // completed answer whose output is published, and exits 1.
    Ok(if stats.creates + stats.modifies + stats.deletes > 0 {
        Outcome::Negative
    } else {
        Outcome::Positive
    })
}

fn run_getid(
    input: &pbfhogg::read::input::PinnedInput,
    output: pbfhogg::Output,
    flags: GetidFlags,
    sources: IdSources,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &pbfhogg::host::Host,
) -> Result<(), Box<dyn std::error::Error>> {
    let GetidFlags {
        add_referenced,
        remove_tags,
        verbose_ids,
    } = flags;
    let id_set = sources.resolve(write.direct_reads, host)?;

    if verbose_ids {
        print_requested_ids(&id_set);
    }

    let opts = pbfhogg::getid::GetidOptions {
        add_referenced,
        remove_tags,
        report_found: verbose_ids,
    };
    let stats = pbfhogg::getid::getid(input, output, &id_set, &opts, write, overrides, host)?;
    stats.write_summary(&mut host.diagnostics.summary_writer("getid.summary"))?;
    host::note_unverified_claims(stats.passthrough_claimed);

    if let Some(found) = stats.found.as_ref() {
        print_missing_ids(&id_set, found);
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_getparents(
    input: &pbfhogg::read::input::PinnedInput,
    output: pbfhogg::Output,
    add_self: bool,
    sources: IdSources,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    full_scan_min_blobs: Option<u64>,
    host: &pbfhogg::host::Host,
) -> Result<(), Box<dyn std::error::Error>> {
    let id_set = sources.resolve(write.direct_reads, host)?;
    let opts = pbfhogg::getparents::GetparentsOptions { add_self };
    let stats = if let Some(min_blobs) = full_scan_min_blobs {
        pbfhogg::getparents::getparents_with_min_blobs(
            input, output, &id_set, &opts, write, overrides, min_blobs, host,
        )?
    } else {
        pbfhogg::getparents::getparents(input, output, &id_set, &opts, write, overrides, host)?
    };
    stats.write_summary(&mut host.diagnostics.summary_writer("getparents.summary"))?;
    Ok(())
}

fn run_removeid(
    input: &pbfhogg::read::input::PinnedInput,
    output: pbfhogg::Output,
    sources: IdSources,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &pbfhogg::host::Host,
) -> Result<(), Box<dyn std::error::Error>> {
    let id_set = sources.resolve(write.direct_reads, host)?;
    let stats = pbfhogg::getid::removeid(input, output, &id_set, write, overrides, host)?;
    stats.write_summary(&mut host.diagnostics.summary_writer("getid.summary"))?;
    host::note_unverified_claims(stats.passthrough_claimed);
    Ok(())
}

fn extract_strategy(simple: bool, smart: bool) -> pbfhogg::extract::ExtractStrategy {
    if simple {
        pbfhogg::extract::ExtractStrategy::Simple
    } else if smart {
        pbfhogg::extract::ExtractStrategy::Smart
    } else {
        pbfhogg::extract::ExtractStrategy::CompleteWays
    }
}

/// The single-extract region. clap made `--bbox` and `--polygon` a required,
/// exclusive pair and already parsed the bbox; a polygon file is read here.
fn extract_region(
    bbox: Option<pbfhogg::extract::Bbox>,
    polygon_path: Option<&std::path::Path>,
) -> Result<pbfhogg::extract::Region, Box<dyn std::error::Error>> {
    match (bbox, polygon_path) {
        (Some(bbox), None) => Ok(pbfhogg::extract::Region::Bbox(bbox)),
        (None, Some(p)) => Ok(pbfhogg::extract::parse_geojson(p)?),
        (None, None) => Err("one of --bbox or --polygon is required".into()),
        (Some(_), Some(_)) => Err("--bbox and --polygon are mutually exclusive".into()),
    }
}

#[allow(clippy::too_many_arguments)]
fn run_extract(
    file: &std::path::Path,
    output: pbfhogg::Output,
    region: &pbfhogg::extract::Region,
    strategy: pbfhogg::extract::ExtractStrategy,
    set_bounds: bool,
    clean: &CleanAttrs,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &pbfhogg::host::Host,
) -> Result<(), Box<dyn std::error::Error>> {
    let stats = pbfhogg::extract::extract(
        file, output, region, strategy, set_bounds, clean, write, overrides, host,
    )?;
    stats.write_summary(&mut host.diagnostics.summary_writer("extract.summary"))?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_extract_config(
    file: &std::path::Path,
    config_path: &std::path::Path,
    directory_override: Option<&std::path::Path>,
    overwrite: pbfhogg::Overwrite,
    strategy: pbfhogg::extract::ExtractStrategy,
    set_bounds: bool,
    clean: &CleanAttrs,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &pbfhogg::host::Host,
    leftovers: &mut LeftoverScanner,
) -> Result<(), Box<dyn std::error::Error>> {
    let (_, slots) = pbfhogg::extract::parse_extract_config_with_output_directory(
        config_path,
        directory_override,
    )?;

    // Every input the run reads: the PBF, the config itself, and each
    // `polygon_file` the config names.
    for slot in &slots {
        let mut inputs: Vec<&std::path::Path> = vec![file, config_path];
        inputs.extend(slots.iter().filter_map(|s| s.polygon_file.as_deref()));
        reject_aliases(&inputs, &slot.output)?;
    }
    // The config names the outputs, so only now are their directories known.
    // Scanned before any output is reserved, each directory once.
    for slot in &slots {
        leftovers.dir(&parent_of(&slot.output), host);
    }
    // Every output the config names is checked here, before any extraction:
    // an existing one (without --overwrite), a symlink, two slots resolving
    // to one file. A config with many regions never fails on a late one
    // after the early ones were extracted.
    let outputs = pbfhogg::OutputSet::files(
        slots.iter().map(|slot| &slot.output),
        overwrite,
        &host.diagnostics,
    )?;
    let destinations: Vec<PathBuf> = outputs
        .iter()
        .map(|output| output.destination().to_path_buf())
        .collect();
    let regions: Vec<pbfhogg::extract::Region> =
        slots.into_iter().map(|slot| slot.region).collect();

    host::cli_progress(
        "extract.multi.start",
        format_args!("Multi-extract: {} extracts from config", regions.len()),
    );
    // Extraction is all-or-nothing, and so, as far as failures allow, is
    // publication: the set is published as a group, rolling back members
    // already installed if a later one fails.
    let all_stats = pbfhogg::extract::extract_multi(
        file, &regions, outputs, strategy, set_bounds, clean, write, overrides, host,
    )?;
    for (i, stats) in all_stats.iter().enumerate() {
        // One summary writer per region, the region's label written into it
        // ahead of the summary, so the label and the summary's first line
        // are one event and `--quiet` drops both.
        let mut summary = host.diagnostics.summary_writer("extract.summary");
        write!(summary, "  [{}] {} - ", i + 1, destinations[i].display())?;
        stats.write_summary(&mut summary)?;
    }
    Ok(())
}

fn run_add_locations_to_ways(
    file: &std::path::Path,
    output: pbfhogg::Output,
    options: &pbfhogg::altw::AltwOptions,
    overrides: &HeaderOverrides,
    host: &pbfhogg::host::Host,
    scratch: pbfhogg::scratch::RunScratch,
) -> Result<(), Box<dyn std::error::Error>> {
    let stats =
        pbfhogg::altw::add_locations_to_ways(file, output, options, overrides, host, scratch)?;
    stats.write_summary(&mut host.diagnostics.summary_writer("altw.summary"))?;
    Ok(())
}

fn run_time_filter(
    file: &std::path::Path,
    output: pbfhogg::Output,
    cutoff: i64,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &pbfhogg::host::Host,
) -> Result<(), Box<dyn std::error::Error>> {
    let stats = pbfhogg::time_filter::time_filter(file, output, cutoff, write, overrides, host)?;
    stats.write_summary(&mut host.diagnostics.summary_writer("time_filter.summary"))?;
    Ok(())
}

fn run_show_element(
    path: &std::path::Path,
    element_ref: ElementRef,
    direct_io: bool,
    host: &pbfhogg::host::Host,
) -> Result<Outcome, Box<dyn std::error::Error>> {
    let mut out = output::buffered();
    let found = pbfhogg::inspect::show_element_to(
        path,
        element_ref.kind,
        element_ref.id,
        direct_io,
        &mut out,
        host,
    )?;
    out.flush()?;
    Ok(if found {
        Outcome::Positive
    } else {
        Outcome::Negative
    })
}

/// Which of `run_inspect`'s four execution shapes a flag combination
/// resolves to, decided once up front.
///
/// The distinction is *alone vs combined*, and it is a live one: clap
/// rejects `--show` alongside anything and `-j`/`--force` outside
/// `--nodes`, but `--indexed --extended` and friends remain legal - a
/// combined run does the full scan and folds the extra reports in, while
/// a flag alone gets its fast path that skips the scan entirely.
enum InspectMode<'a> {
    /// `--indexed` alone: header probe only, no full inspect.
    IndexedOnly,
    /// `--nodes` alone: node stats only, no full inspect.
    NodesOnly,
    /// `--get` with a header/file key and nothing else: read just the
    /// header blob.
    HeaderGet {
        key: &'a str,
        known: Option<pbfhogg::inspect::InspectGetKeyKind>,
    },
    /// Everything else: full scan, with any requested extras folded in.
    Full,
}

impl<'a> InspectMode<'a> {
    fn resolve(
        flags: &InspectFlags,
        blocks: Option<usize>,
        get: Option<&'a str>,
        get_lookup: Option<pbfhogg::inspect::InspectGetLookup>,
    ) -> Self {
        let scan_extras = flags.extended
            || flags.id_ranges
            || flags.locations
            || flags.anomalies
            || blocks.is_some();
        if flags.indexed && !scan_extras && !flags.nodes && get.is_none() {
            return Self::IndexedOnly;
        }
        if flags.nodes && !scan_extras && !flags.indexed && get.is_none() {
            return Self::NodesOnly;
        }
        if let Some(key) = get
            && get_lookup
                .is_some_and(|lookup| lookup.need() == pbfhogg::inspect::InspectGetNeed::Header)
            && !scan_extras
            && !flags.nodes
            && !flags.indexed
        {
            return Self::HeaderGet {
                key,
                known: get_lookup.and_then(pbfhogg::inspect::InspectGetLookup::kind),
            };
        }
        Self::Full
    }
}

#[allow(clippy::too_many_lines)]
fn run_inspect(
    path: &std::path::Path,
    flags: InspectFlags,
    jobs: pbfhogg::Parallelism,
    blocks: Option<usize>,
    get: Option<&str>,
    direct_io: bool,
    host: &pbfhogg::host::Host,
) -> Result<Outcome, Box<dyn std::error::Error>> {
    let get_lookup = get.map(pbfhogg::inspect::inspect_get_lookup);
    let mode = InspectMode::resolve(&flags, blocks, get, get_lookup);
    let InspectFlags {
        indexed,
        nodes,
        id_ranges,
        locations,
        anomalies,
        extended,
        json,
        force,
        distrust_metadata,
        quiet,
    } = flags;

    // `--distrust-metadata` only means something for the scanning report:
    // `--indexed` asks whether indexdata is present (distrusting its contents
    // does not change the answer), `--nodes` decodes every node already, and
    // Header and file get paths read without the inspect scan. Rejecting says so
    // rather than accepting a flag that would do nothing.
    // Decided before the input is touched. (`--json` with `--nodes` or
    // `--get`, which have no JSON shape, is refused by clap.) The key table is
    // the whole set `get_value` answers, so an unknown key is refused here
    // rather than after a scan.
    if let (Some(key), Some(lookup)) = (get, get_lookup)
        && lookup.kind().is_none()
    {
        return Err(usage(&["inspect"], format!("unknown key: {key}")));
    }
    if distrust_metadata && !matches!(mode, InspectMode::Full) {
        return Err(usage(
            &["inspect"],
            "--distrust-metadata applies to the scanning report only, not to --indexed, --nodes \
             or a header/file --get key",
        ));
    }

    match mode {
        InspectMode::IndexedOnly => {
            // Reads frame headers up to the first data blob, checking the
            // token before each; decided before and after like every
            // read-only command.
            host.cancel.check()?;
            let has_index = pbfhogg::has_indexdata_in(path, direct_io, host)?;
            host.cancel.check()?;
            let mut out = output::buffered();
            let printed = if quiet {
                Ok(())
            } else if json {
                writeln!(out, "{}", serde_json::json!({"indexed": has_index}))
            } else if has_index {
                writeln!(out, "Indexed: yes")
            } else {
                writeln!(out, "Indexed: no")
            };
            // The answer is the exit status; a reader that left early must
            // not cost it.
            output::tolerate_reader_gone(printed.and_then(|()| out.flush()))?;
            return Ok(if has_index {
                Outcome::Positive
            } else {
                Outcome::Negative
            });
        }
        InspectMode::NodesOnly => {
            let report =
                pbfhogg::inspect::node_stats::node_stats(path, direct_io, force, jobs, host)?;
            let mut out = output::buffered();
            report.write_report(&mut out)?;
            out.flush()?;
            return Ok(Outcome::Positive);
        }
        InspectMode::HeaderGet { key, known } => {
            // One open: the header and the file size both come from it, so
            // `file.size` is the size of the file the header was read from.
            // A file without a header is the library's single missing-header
            // error.
            host.cancel.check()?;
            let read = pbfhogg::inspect::inspect_header(path, direct_io, host)?;
            let header = &read.header;
            let val = match known {
                Some(pbfhogg::inspect::InspectGetKeyKind::FileName) => Some(
                    path.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path.display().to_string()),
                ),
                Some(pbfhogg::inspect::InspectGetKeyKind::FileSize) => {
                    Some(read.file_size.to_string())
                }
                Some(pbfhogg::inspect::InspectGetKeyKind::FileFormat) => Some("PBF".to_string()),
                Some(pbfhogg::inspect::InspectGetKeyKind::HeaderBbox) => header
                    .bbox()
                    .map(|bb| format!("{} {} {} {}", bb.left, bb.bottom, bb.right, bb.top)),
                Some(pbfhogg::inspect::InspectGetKeyKind::HeaderWritingProgram) => {
                    header.writing_program().map(String::from)
                }
                Some(pbfhogg::inspect::InspectGetKeyKind::HeaderReplicationUrl) => {
                    header.osmosis_replication_base_url().map(String::from)
                }
                Some(pbfhogg::inspect::InspectGetKeyKind::HeaderReplicationSequence) => header
                    .osmosis_replication_sequence_number()
                    .map(|s| s.to_string()),
                Some(pbfhogg::inspect::InspectGetKeyKind::HeaderReplicationTimestamp) => header
                    .osmosis_replication_timestamp()
                    .map(|t| t.to_string()),
                _ => return Err(format!("unknown key: {key}").into()),
            };
            let Some(v) = val else {
                return Err(format!("key {key} has no value in this file").into());
            };
            // The completion decision, as for `--indexed`.
            host.cancel.check()?;
            let mut out = output::buffered();
            writeln!(out, "{v}")?;
            out.flush()?;
            return Ok(Outcome::Positive);
        }
        InspectMode::Full => {}
    }

    // The get-key table marks data and metadata paths as requiring extended stats.
    let extended = extended
        || get_lookup
            .is_some_and(|lookup| lookup.need() == pbfhogg::inspect::InspectGetNeed::Extended);
    let show_blocks = blocks.is_some() || anomalies;
    let block_limit = if anomalies && blocks.is_none() {
        Some(0)
    } else {
        blocks
    };
    // With `--indexed` or `--nodes` the run is several library calls on one
    // input (the scan, the indexdata probe, the node statistics), each
    // opening it afresh, so the input is pinned here, before any of them,
    // and each reads through the pin: a file renamed over the input mid-run
    // cannot answer one question from each file. Alone, the scan decides for
    // itself (it pins when it may read twice), so a full-decode-only scan
    // still reads a FIFO.
    host.cancel.check()?;
    let pin = if indexed || nodes {
        Some(pbfhogg::read::input::PinnedInput::pin(path, host)?)
    } else {
        None
    };
    let input: pbfhogg::read::input::InputSource<'_> = pin.as_ref().map_or(path.into(), Into::into);
    let mut report = pbfhogg::inspect::inspect(
        input,
        &pbfhogg::inspect::InspectOptions {
            show_blocks,
            show_id_ranges: id_ranges,
            show_locations: locations,
            extended,
            distrust_metadata,
            direct_io,
        },
        host,
    )?;

    // --indexed combined with other flags: check and report, set exit code
    let has_index = if indexed {
        Some(pbfhogg::has_indexdata_in(input, direct_io, host)?)
    } else {
        None
    };
    let exit_not_indexed = has_index == Some(false);

    // --nodes combined with other flags: run node stats, printed below
    let node_report = if nodes {
        Some(pbfhogg::inspect::node_stats::node_stats(
            input, direct_io, force, jobs, host,
        )?)
    } else {
        None
    };
    // The completion decision: the indexdata probe has no checkpoint of its
    // own, so a cancellation during it must not still produce an answer.
    host.cancel.check()?;

    // Everything is computed; only output remains. `--indexed` makes the
    // exit status an answer, and a reader that left early must not cost it.
    let printed = (|| -> Result<(), Box<dyn std::error::Error>> {
        let mut out = output::buffered();
        if let Some(has_index) = has_index
            && !json
        {
            writeln!(out, "Indexed: {}", if has_index { "yes" } else { "no" })?;
        }
        if let Some(ref node_report) = node_report {
            node_report.write_report(&mut out)?;
        }
        if let Some(key) = get {
            match report.get_value(key) {
                Some(val) => writeln!(out, "{val}")?,
                None => return Err(format!("unknown key: {key}").into()),
            }
        } else if json {
            let mut value = report.to_json_filtered(block_limit, anomalies);
            if indexed && let serde_json::Value::Object(ref mut map) = value {
                map.insert(
                    "indexed".to_string(),
                    serde_json::Value::Bool(!exit_not_indexed),
                );
            }
            writeln!(out, "{value}")?;
        } else {
            report.write_report_filtered(&mut out, block_limit, anomalies)?;
        }
        out.flush()?;
        Ok(())
    })();
    output::tolerate_reader_gone(printed)?;

    Ok(if exit_not_indexed {
        Outcome::Negative
    } else {
        Outcome::Positive
    })
}

fn run_apply_changes(
    base: &std::path::Path,
    changes: &std::path::Path,
    output: pbfhogg::Output,
    opts: &pbfhogg::apply_changes::MergeOptions,
    overrides: &HeaderOverrides,
    host: &pbfhogg::host::Host,
) -> Result<(), Box<dyn std::error::Error>> {
    let stats = pbfhogg::apply_changes::merge(base, changes, output, opts, overrides, host)?;
    stats.write_summary(&mut host.diagnostics.summary_writer("apply_changes.summary"))?;
    Ok(())
}

fn run_merge_pbf(
    inputs: &[PathBuf],
    output: pbfhogg::Output,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &pbfhogg::host::Host,
) -> Result<(), Box<dyn std::error::Error>> {
    let paths: Vec<&std::path::Path> = inputs.iter().map(AsRef::as_ref).collect();
    let stats = pbfhogg::cat::dedupe::merge_pbf(&paths, output, write, overrides, host)?;
    stats.write_summary(&mut host.diagnostics.summary_writer("cat.dedupe.summary"))?;
    host::note_unverified_claims(stats.passthrough_claimed);
    Ok(())
}

fn run_merge_changes(
    changes: &[PathBuf],
    output: pbfhogg::Output,
    simplify: bool,
    jobs: pbfhogg::Parallelism,
    allow_version_regression: bool,
    host: &pbfhogg::host::Host,
) -> Result<(), Box<dyn std::error::Error>> {
    let inputs: Vec<&std::path::Path> = changes.iter().map(AsRef::as_ref).collect();
    let stats = pbfhogg::merge_changes::merge_changes(
        &inputs,
        output,
        simplify,
        jobs,
        allow_version_regression,
        host,
    )?;
    stats.write_summary(&mut host.diagnostics.summary_writer("merge_changes.summary"))?;
    Ok(())
}

fn run_bench_read(
    path: &std::path::Path,
    mode: BenchReadMode,
    host: &pbfhogg::host::Host,
) -> Result<(), Box<dyn std::error::Error>> {
    // A plain Instant, not the library's crate-private counter Stopwatch: the
    // elapsed_ms printed below is bench-read output the benchmark harness
    // parses, not a sidecar counter, and its whole-millisecond value must not
    // change.
    use std::time::Instant;

    let (elapsed_ms, nodes, ways, rels): (u128, u64, u64, u64) = match mode {
        BenchReadMode::Sequential => {
            let reader = pbfhogg::ElementReader::from_path_with_eviction(path, true)?
                .diagnostics(host.diagnostics.clone())
                .cancel(host.cancel.clone());
            let mut nodes = 0u64;
            let mut ways = 0u64;
            let mut rels = 0u64;
            let start = Instant::now();
            reader.for_each(|el| match el {
                pbfhogg::Element::Node(_) | pbfhogg::Element::DenseNode(_) => nodes += 1,
                pbfhogg::Element::Way(_) => ways += 1,
                pbfhogg::Element::Relation(_) => rels += 1,
                _ => {}
            })?;
            (start.elapsed().as_millis(), nodes, ways, rels)
        }
        BenchReadMode::Parallel => {
            let reader = pbfhogg::ElementReader::from_path_with_eviction(path, true)?
                .diagnostics(host.diagnostics.clone())
                .cancel(host.cancel.clone());
            let start = Instant::now();
            let (nodes, ways, rels) = reader.par_map_reduce(
                |el| match el {
                    pbfhogg::Element::Node(_) | pbfhogg::Element::DenseNode(_) => {
                        (1u64, 0u64, 0u64)
                    }
                    pbfhogg::Element::Way(_) => (0, 1, 0),
                    pbfhogg::Element::Relation(_) => (0, 0, 1),
                    _ => (0, 0, 0),
                },
                || (0, 0, 0),
                |a, b| (a.0 + b.0, a.1 + b.1, a.2 + b.2),
            )?;
            (start.elapsed().as_millis(), nodes, ways, rels)
        }
        BenchReadMode::Pipelined => {
            let reader = pbfhogg::ElementReader::from_path_with_eviction(path, true)?
                .diagnostics(host.diagnostics.clone())
                .cancel(host.cancel.clone());
            let mut nodes = 0u64;
            let mut ways = 0u64;
            let mut rels = 0u64;
            let start = Instant::now();
            reader.for_each_pipelined(|el| match el {
                pbfhogg::Element::Node(_) | pbfhogg::Element::DenseNode(_) => nodes += 1,
                pbfhogg::Element::Way(_) => ways += 1,
                pbfhogg::Element::Relation(_) => rels += 1,
                _ => {}
            })?;
            (start.elapsed().as_millis(), nodes, ways, rels)
        }
        BenchReadMode::Blobreader => {
            // Route through the library's buffered path (256 KiB FileReader)
            // so this arm measures decode architecture, not the syscall
            // overhead of a default 8 KiB BufReader.
            let mut reader = pbfhogg::BlobReader::from_path(path)?
                .evict_page_cache(true)
                .diagnostics(host.diagnostics.clone())
                .cancel(host.cancel.clone());
            let source: std::sync::Arc<std::path::Path> = std::sync::Arc::from(path);
            // The leading OsmHeader is required like everywhere else: a
            // zero-byte or header-less file is the one missing-header error,
            // not an empty run. Taken and decoded before the timer, as the
            // other modes' ElementReader does at open.
            let header = reader.leading_header()?;
            header
                .decode()
                .map_err(|e| header.locate_error(Some(&source), e))?;
            let mut nodes = 0u64;
            let mut ways = 0u64;
            let mut rels = 0u64;
            let start = Instant::now();
            for blob_result in reader {
                let blob = blob_result?;
                let decoded = blob
                    .decode()
                    .map_err(|e| blob.locate_error(Some(&source), e))?;
                if let pbfhogg::BlobDecode::OsmData(block) = decoded {
                    for el in block.elements() {
                        match el {
                            pbfhogg::Element::Node(_) | pbfhogg::Element::DenseNode(_) => {
                                nodes += 1;
                            }
                            pbfhogg::Element::Way(_) => ways += 1,
                            pbfhogg::Element::Relation(_) => rels += 1,
                            _ => {}
                        }
                    }
                }
            }
            (start.elapsed().as_millis(), nodes, ways, rels)
        }
    };

    output::cli_eprintln!("elapsed_ms={elapsed_ms}");
    output::cli_eprintln!("nodes={nodes}");
    output::cli_eprintln!("ways={ways}");
    output::cli_eprintln!("relations={rels}");
    Ok(())
}

fn bench_write_loop<W: std::io::Write>(
    path: &std::path::Path,
    writer: &mut pbfhogg::writer::PbfWriter<W>,
    host: &pbfhogg::host::Host,
) -> Result<(u64, u64, u64), Box<dyn std::error::Error>> {
    use pbfhogg::block_builder::{BlockBuilder, MemberData};
    use std::io::BufReader;

    // The same default-capacity BufReader this benchmark has always read
    // through, named so its errors carry the file and byte offset.
    let mut reader = pbfhogg::BlobReader::new_named(BufReader::new(open_input(path)?), path)
        .diagnostics(host.diagnostics.clone())
        .cancel(host.cancel.clone());
    // The leading OsmHeader is required like everywhere else: a zero-byte or
    // header-less file is the one missing-header error, not an empty run.
    // Taken from this pass's own reader rather than a second open, and
    // decoded so a corrupt header still fails before any data blob.
    let header = reader.leading_header()?;
    header
        .decode()
        .map_err(|e| header.locate_error(Some(&std::sync::Arc::from(path)), e))?;
    let mut bb = BlockBuilder::new();
    let mut nodes = 0u64;
    let mut ways = 0u64;
    let mut rels = 0u64;

    for blob_result in reader {
        let blob = blob_result?;
        // Decode errors carry no location of their own; the path is built
        // only on that error path.
        let decoded = blob
            .decode()
            .map_err(|e| blob.locate_error(Some(&std::sync::Arc::from(path)), e))?;
        if let pbfhogg::BlobDecode::OsmData(block) = decoded {
            for el in block.elements() {
                match el {
                    pbfhogg::Element::DenseNode(dn) => {
                        if !bb.can_add_node()
                            && let Some(bytes) = bb.take()?
                        {
                            writer.write_primitive_block(bytes)?;
                        }
                        bb.add_node(
                            dn.id(),
                            dn.decimicro_lat(),
                            dn.decimicro_lon(),
                            dn.tags(),
                            None,
                        );
                        nodes += 1;
                    }
                    pbfhogg::Element::Node(n) => {
                        if !bb.can_add_node()
                            && let Some(bytes) = bb.take()?
                        {
                            writer.write_primitive_block(bytes)?;
                        }
                        bb.add_node(n.id(), n.decimicro_lat(), n.decimicro_lon(), n.tags(), None);
                        nodes += 1;
                    }
                    pbfhogg::Element::Way(w) => {
                        if !bb.can_add_way()
                            && let Some(bytes) = bb.take()?
                        {
                            writer.write_primitive_block(bytes)?;
                        }
                        let refs: Vec<_> = w.refs().collect();
                        bb.add_way(w.id(), w.tags(), &refs, None);
                        ways += 1;
                    }
                    pbfhogg::Element::Relation(r) => {
                        if !bb.can_add_relation()
                            && let Some(bytes) = bb.take()?
                        {
                            writer.write_primitive_block(bytes)?;
                        }
                        let members: Vec<_> = r
                            .members()
                            .map(|m| MemberData {
                                id: m.id,
                                role: m.role().ok().unwrap_or_default(),
                            })
                            .collect();
                        bb.add_relation(r.id(), r.tags(), &members, None);
                        rels += 1;
                    }
                    _ => {}
                }
            }
        }
    }

    if let Some(bytes) = bb.take()? {
        writer.write_primitive_block(bytes)?;
    }

    Ok((nodes, ways, rels))
}

fn run_bench_write(
    path: &std::path::Path,
    compression: Compression,
    writer_mode: BenchWriterMode,
    host: &pbfhogg::host::Host,
) -> Result<(), Box<dyn std::error::Error>> {
    use pbfhogg::block_builder::HeaderBuilder;
    use std::time::Instant;

    let header_bytes = HeaderBuilder::new().build()?;

    let (elapsed_ms, nodes, ways, rels) = match writer_mode {
        BenchWriterMode::Sync => {
            // A sink, not an output: the benchmark measures encoding and
            // discards the bytes.
            #[expect(
                clippy::disallowed_methods,
                reason = "/dev/null is a byte sink, not a published output"
            )]
            let file = std::fs::File::create("/dev/null")?;
            let buf = std::io::BufWriter::with_capacity(256 * 1024, file);
            let mut writer = pbfhogg::writer::PbfWriter::new(buf, compression);
            writer.write_header(&header_bytes)?;
            let start = Instant::now();
            let (nodes, ways, rels) = bench_write_loop(path, &mut writer, host)?;
            // Flush in the timed window: the tail of the BufWriter is part of
            // the write, and its error is the run's error. Teardown stays
            // outside the window, as it always was for this arm.
            writer.flush()?;
            let elapsed_ms = start.elapsed().as_millis();
            (elapsed_ms, nodes, ways, rels)
        }
        BenchWriterMode::Pipelined => {
            let mut writer = pbfhogg::writer::PbfWriter::to_path(
                std::path::Path::new("/dev/null"),
                compression,
                &header_bytes,
                pbfhogg::host::Executor::Ambient,
            )?;
            let start = Instant::now();
            let (nodes, ways, rels) = bench_write_loop(path, &mut writer, host)?;
            // `flush`, not just `drop`: Drop joins the writer thread too, but
            // discards its result and skips the end-of-stream completeness
            // check. The drop stays inside the window so tearing the writer
            // down is still timed as it always was.
            writer.flush()?;
            drop(writer);
            let elapsed_ms = start.elapsed().as_millis();
            (elapsed_ms, nodes, ways, rels)
        }
    };

    output::cli_eprintln!("elapsed_ms={elapsed_ms}");
    output::cli_eprintln!("nodes={nodes}");
    output::cli_eprintln!("ways={ways}");
    output::cli_eprintln!("relations={rels}");
    Ok(())
}

#[allow(clippy::cast_precision_loss)]
fn run_bench_merge(
    base: &std::path::Path,
    changes: &std::path::Path,
    output: pbfhogg::Output,
    compression: Compression,
    io_mode: BenchIoMode,
    host: &pbfhogg::host::Host,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::time::Instant;

    let backend = OutputBackend::from_flags(false, matches!(io_mode, BenchIoMode::Uring));
    let destination = output.destination().to_path_buf();

    // The timer stops where the merge, its write and its flush end: the
    // library publishes the output (fsync, rename, directory sync) inside
    // `merge`, and that is not part of the measured work, so the output stops
    // the clock itself when publication begins.
    let start = Instant::now();
    let work_elapsed = std::sync::Arc::new(std::sync::OnceLock::new());
    let output = {
        let work_elapsed = std::sync::Arc::clone(&work_elapsed);
        output.before_publish(move || {
            work_elapsed.set(start.elapsed()).ok();
        })
    };
    let opts = pbfhogg::apply_changes::MergeOptions {
        write: WriteOptions {
            compression,
            backend,
            direct_reads: false,
            force: true,
        },
        locations_on_ways: false,
        ignore_missing_nodes: false,
        parallelism: pbfhogg::Parallelism::Auto,
        allow_version_regression: false,
    };
    let stats = pbfhogg::apply_changes::merge(
        base,
        changes,
        output,
        &opts,
        &HeaderOverrides::default(),
        host,
    )?;
    let elapsed_ms = work_elapsed
        .get()
        .ok_or("bench-merge: the output was published without stopping the timer")?
        .as_millis();

    let output_mb = std::fs::metadata(&destination)
        .map(|m| m.len() as f64 / 1_000_000.0)
        .unwrap_or(0.0);

    output::cli_eprintln!("elapsed_ms={elapsed_ms}");
    output::cli_eprintln!("base_nodes={}", stats.base_nodes);
    output::cli_eprintln!("base_ways={}", stats.base_ways);
    output::cli_eprintln!("base_relations={}", stats.base_relations);
    output::cli_eprintln!("diff_nodes={}", stats.diff_nodes);
    output::cli_eprintln!("diff_ways={}", stats.diff_ways);
    output::cli_eprintln!("diff_relations={}", stats.diff_relations);
    output::cli_eprintln!("blobs_passthrough={}", stats.blobs_passthrough);
    output::cli_eprintln!("blobs_rewritten={}", stats.blobs_rewritten);
    output::cli_eprintln!("bytes_passthrough={}", stats.bytes_passthrough);
    output::cli_eprintln!("bytes_rewritten={}", stats.bytes_rewritten);
    output::cli_eprintln!("diff_heap_bytes={}", stats.diff_heap_bytes);
    output::cli_eprintln!("output_mb={output_mb:.2}");
    // Peak RSS is a best-effort diagnostic; omit the field when the probe
    // cannot provide a reading instead of printing a fabricated zero.
    if let Some(peak_kb) = pbfhogg::process::read_peak_rss_kb() {
        output::cli_eprintln!("peak_rss_kb={peak_kb}");
    }
    Ok(())
}

/// The short-flag vocabulary: every short letter has one meaning across all
/// subcommands. `reference/cli-reference.md` "Short flags" is the same table
/// for readers, and a test keeps the two equal.
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod short_flag_tests {
    use super::{
        RETIRED_LONGS, RETIRED_SHORTS, cli_command, parse_export_type, parse_single_type,
        parse_type_filter,
    };

    /// Letter, then the long flag(s) it stands for. `-o` is the output
    /// location: `--output`, or build-geocode-index's `--output-dir`.
    const VOCABULARY: &[(char, &[&str])] = &[
        ('b', &["bbox"]),
        ('c', &["suppress-common"]),
        ('d', &["directory"]),
        ('e', &["expressions"]),
        ('g', &["get"]),
        ('h', &["help"]),
        ('i', &["id-file"]),
        ('I', &["id-osm-file"]),
        ('j', &["jobs"]),
        ('M', &["max-count"]),
        ('o', &["output", "output-dir"]),
        ('O', &["overwrite"]),
        ('p', &["polygon"]),
        ('q', &["quiet"]),
        ('r', &["add-referenced"]),
        ('R', &["omit-referenced"]),
        ('s', &["osmium-summary"]),
        ('t', &["type"]),
        ('v', &["verbose"]),
        ('V', &["version"]),
    ];

    fn is_retired(arg: &clap::Arg) -> bool {
        arg.get_id().as_str().starts_with("retired-short-")
    }

    /// Every (command path, short, long) in the built tree, retired shorts
    /// excluded.
    fn shorts() -> Vec<(String, char, String)> {
        fn walk(cmd: &clap::Command, path: &str, out: &mut Vec<(String, char, String)>) {
            for arg in cmd.get_arguments() {
                if is_retired(arg) {
                    continue;
                }
                let long = arg.get_long().unwrap_or("<none>").to_owned();
                for short in arg
                    .get_short()
                    .into_iter()
                    .chain(arg.get_all_short_aliases().unwrap_or_default())
                {
                    out.push((path.to_owned(), short, long.clone()));
                }
            }
            for sub in cmd.get_subcommands() {
                walk(sub, &format!("{path} {}", sub.get_name()), out);
            }
        }
        let mut cmd = cli_command();
        cmd.build();
        let mut out = Vec::new();
        walk(&cmd, "pbfhogg", &mut out);
        out
    }

    #[test]
    fn the_command_tree_is_well_formed() {
        cli_command().debug_assert();
    }

    #[test]
    fn every_short_flag_has_one_meaning_across_all_subcommands() {
        let mut problems = Vec::new();
        for (path, short, long) in shorts() {
            match VOCABULARY.iter().find(|(letter, _)| *letter == short) {
                None => problems.push(format!(
                    "`{path}` uses -{short} (--{long}), which the vocabulary does not define"
                )),
                Some((_, longs)) if !longs.contains(&long.as_str()) => problems.push(format!(
                    "`{path}` uses -{short} for --{long}; in the vocabulary -{short} is --{}",
                    longs.join(" / --")
                )),
                Some(_) => {}
            }
        }
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    }

    #[test]
    fn every_vocabulary_letter_is_used() {
        let used = shorts();
        for (letter, _) in VOCABULARY {
            assert!(
                used.iter().any(|(_, short, _)| short == letter),
                "-{letter} is in the vocabulary but no command uses it"
            );
        }
    }

    /// A command line that parses, per retired-short command path.
    fn valid_argv(path: &[&str]) -> Vec<&'static str> {
        match path {
            ["cat"] => vec!["cat", "in.pbf", "-o", "out.pbf"],
            ["extract"] => vec!["extract", "in.pbf", "-b", "0,0,1,1", "-o", "out.pbf"],
            ["renumber"] => vec!["renumber", "in.pbf", "-o", "out.pbf"],
            ["getparents"] => vec!["getparents", "in.pbf", "-o", "out.pbf", "n1"],
            ["getid"] => vec!["getid", "in.pbf", "-o", "out.pbf", "n1"],
            ["inspect", "tags"] => vec!["inspect", "tags", "in.pbf"],
            ["inspect"] => vec!["inspect", "in.pbf"],
            ["tags-filter"] => vec!["tags-filter", "in.pbf", "-o", "out.pbf", "highway"],
            other => panic!("no valid command line for {other:?}: add one"),
        }
    }

    fn parse(argv: &[&str]) -> Result<(), String> {
        cli_command()
            .try_get_matches_from(std::iter::once("pbfhogg").chain(argv.iter().copied()))
            .map(drop)
            .map_err(|e| e.render().to_string())
    }

    #[test]
    fn a_retired_short_is_refused_naming_its_long_flag_and_only_when_given() {
        for retired in RETIRED_SHORTS {
            let valid = valid_argv(retired.path);
            parse(&valid).unwrap_or_else(|e| {
                panic!(
                    "{:?} without -{} must parse:\n{e}",
                    retired.path, retired.short
                )
            });
            let flag = format!("-{}", retired.short);
            // Bare, as a flag; and followed by a value, as an option.
            let bare: Vec<&str> = valid.iter().copied().chain([flag.as_str()]).collect();
            let with_value: Vec<&str> = valid.iter().copied().chain([flag.as_str(), "5"]).collect();
            for argv in [bare, with_value] {
                let err = parse(&argv).expect_err(&format!("{argv:?} must be refused"));
                assert!(
                    err.contains(retired.now) && err.contains(&flag),
                    "{argv:?}: the refusal must name {flag} and {}:\n{err}",
                    retired.now
                );
            }
        }
    }

    #[test]
    fn a_retired_long_is_refused_naming_its_replacement() {
        for retired in RETIRED_LONGS {
            let valid = valid_argv(retired.path);
            let flag = format!("--{}", retired.long);
            let argv: Vec<&str> = valid.iter().copied().chain([flag.as_str()]).collect();
            let err = parse(&argv).expect_err("a retired long flag must be refused");
            assert!(
                err.contains(retired.now) && err.contains(&flag),
                "{argv:?}: the refusal must name {flag} and {}:\n{err}",
                retired.now
            );
        }
    }

    #[test]
    fn type_parsers_share_one_grammar_and_refuse_by_command() {
        assert_eq!(
            parse_type_filter("node,way").map(pbfhogg::TypeFilter::count),
            Ok(2)
        );
        assert!(parse_single_type("node,way").is_err());
        assert!(parse_single_type("way").is_ok());
        assert!(parse_export_type("relation").is_err());
        assert!(parse_export_type("node,relation").is_err());
        assert!(matches!(
            parse_export_type("node,way"),
            Ok(pbfhogg::export::ExportTypes::All)
        ));
        assert!(matches!(
            parse_export_type("way"),
            Ok(pbfhogg::export::ExportTypes::WaysOnly)
        ));
    }

    #[test]
    #[allow(clippy::disallowed_methods)] // reads a repository document, not an operator input
    fn the_reference_table_is_the_vocabulary() {
        let doc = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../reference/cli-reference.md"
        ))
        .unwrap();
        let section = doc
            .split("## Short flags")
            .nth(1)
            .expect("reference/cli-reference.md has a `## Short flags` section");
        let section = section.split("\n## ").next().unwrap_or(section);
        let mut documented: Vec<(char, Vec<String>)> = Vec::new();
        for line in section.lines() {
            let cells: Vec<&str> = line.split('|').map(str::trim).collect();
            // | `-x` | `--long` (, `--long`) | ... |
            let [_, short, longs, ..] = cells[..] else {
                continue;
            };
            let Some(letter) = short
                .strip_prefix("`-")
                .and_then(|s| s.strip_suffix('`'))
                .filter(|s| s.chars().count() == 1)
                .and_then(|s| s.chars().next())
            else {
                continue;
            };
            let longs = longs
                .split(',')
                .filter_map(|l| {
                    l.trim()
                        .strip_prefix("`--")
                        .and_then(|l| l.strip_suffix('`'))
                        .map(str::to_owned)
                })
                .collect();
            documented.push((letter, longs));
        }
        let expected: Vec<(char, Vec<String>)> = VOCABULARY
            .iter()
            .map(|(letter, longs)| (*letter, longs.iter().map(|l| (*l).to_owned()).collect()))
            .collect();
        documented.sort();
        let mut expected = expected;
        expected.sort();
        assert_eq!(documented, expected);
    }
}
