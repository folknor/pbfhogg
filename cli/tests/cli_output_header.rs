//! The output-header contract, enforced on every PBF-writing subcommand by one
//! table.
//!
//! The contract is `reference/cli-reference.md` "Output header": a header
//! never claims more than the output contains, and every command derives its
//! header from the same rule (`src/commands/output_header.rs`). For every row
//! of [`ROWS`], over every fixture of [`Fixture`]:
//!
//! - **Exact declarations.** `Sort.Type_then_ID`, `LocationsOnWays`,
//!   `pbfhogg.WayMembers-v1` and `pbfhogg.SharedNodePins-v1` are exactly what
//!   the row expects: a raw-copying command keeps what its copied blobs carry,
//!   a re-encoding one drops it, a command that breaks the order says so.
//! - **Soundness against the content.** Whatever the header declares, the
//!   output is read back and checked: sorted means every element follows its
//!   predecessor in type-then-ID order; `LocationsOnWays` means every way
//!   carries a location per ref; `WayMembers-v1` means every way blob carries
//!   field 5; `SharedNodePins-v1` means every way's pin bitmap is the one the
//!   input carried. A carried bbox contains every node.
//! - **Header fields.** bbox, replication state and writing program are what
//!   the row expects.
//!
//! ## Complete by construction
//!
//! The subcommands come from the binary's own `--help`. One the binary has
//! that is in neither [`ROWS`] nor [`NO_PBF_HEADER`] fails
//! `every_subcommand_is_in_the_table_or_writes_no_pbf_header`, so a new
//! PBF-writing command cannot skip this test.

// Fixture and readback setup unwraps; the property checks report every
// violation at once through `Result<(), String>`.
#![allow(clippy::unwrap_used, clippy::unwrap_in_result)]

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::invoker::{CliInvoker, CliOutput, subcommands};
use pbfhogg::block_builder::{BlockBuilder, HeaderBuilder, MemberData, Metadata};
use pbfhogg::writer::{Compression, PbfWriter};
use pbfhogg::{BlobDecode, BlobReader, Element, HeaderBlock, MemberId};
use tempfile::TempDir;

macro_rules! ensure {
    ($cond:expr, $($arg:tt)*) => {
        if !$cond {
            return Err(format!($($arg)*));
        }
    };
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

const PROGRAM: &str = "fixture-writer";
/// What pbfhogg itself writes: its package name and version.
const OWN_PROGRAM: &str = concat!("pbfhogg/", env!("CARGO_PKG_VERSION"));
const REPLICATION_TS: i64 = 1000;
const REPLICATION_SEQ: i64 = 7;
const REPLICATION_URL: &str = "https://replication.invalid/minute";
/// The fixture's declared box (left, bottom, right, top), on the
/// decimicrodegree grid, containing every node.
const FIXTURE_BBOX: (f64, f64, f64, f64) = (0.0, 0.0, 0.01, 0.01);
/// The extract region: nodes 1..=30.
const REGION: (f64, f64, f64, f64) = (0.0, 0.0, 0.003, 0.003);
const NODES: i64 = 60;
const WAYS: i64 = 20;
const RELATIONS: i64 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fixture {
    /// Sorted, indexed, no way enrichment.
    Plain,
    /// The same elements in descending ID order, header not sorted.
    Unsorted,
    /// `Plain` through add-locations-to-ways: LocationsOnWays.
    Low,
    /// `Plain` through `add-locations-to-ways --inject-prepass`: all three.
    Prepass,
}

impl Fixture {
    fn sorted(self) -> bool {
        self != Self::Unsorted
    }
    fn low(self) -> bool {
        matches!(self, Self::Low | Self::Prepass)
    }
    fn prepass(self) -> bool {
        self == Self::Prepass
    }
    /// add-locations-to-ways writes its own program and version
    /// ([`OWN_PROGRAM`]), so only the
    /// two hand-written fixtures carry [`PROGRAM`].
    fn program(self) -> &'static str {
        if self.low() { OWN_PROGRAM } else { PROGRAM }
    }
}

struct Fx {
    dir: TempDir,
    counter: std::cell::Cell<usize>,
}

impl Fx {
    fn new(fixtures: &[Fixture]) -> Self {
        let dir = crate::common::test_scratch();
        let fx = Self {
            dir,
            counter: std::cell::Cell::new(0),
        };
        write_fixture(&fx.path(Fixture::Plain), true);
        write_fixture(&fx.path(Fixture::Unsorted), false);
        if fixtures.iter().any(|f| f.low()) {
            enrich(&fx.path(Fixture::Plain), &fx.path(Fixture::Low), false);
            enrich(&fx.path(Fixture::Plain), &fx.path(Fixture::Prepass), true);
        }
        std::fs::write(
            fx.dir.path().join("change.osc"),
            b"<osmChange version=\"0.6\"><create>\
              <node id=\"1000\" version=\"1\" timestamp=\"1970-01-01T00:00:10Z\" \
              lat=\"10.0\" lon=\"10.0\"><tag k=\"amenity\" v=\"cafe\"/></node>\
              </create></osmChange>",
        )
        .unwrap();
        fx
    }

    fn path(&self, fixture: Fixture) -> PathBuf {
        self.dir.path().join(format!("{fixture:?}.osm.pbf"))
    }

    fn osc(&self) -> PathBuf {
        self.dir.path().join("change.osc")
    }

    /// A fresh output path.
    fn out(&self, name: &str) -> PathBuf {
        self.counter.set(self.counter.get() + 1);
        self.dir
            .path()
            .join(format!("out-{}-{name}.osm.pbf", self.counter.get()))
    }
}

fn meta() -> Metadata<'static> {
    Metadata {
        version: 1,
        timestamp: 100,
        changeset: 1,
        uid: 1,
        user: "u",
        visible: true,
    }
}

/// Write the fixture by hand so its header carries every field the policy
/// handles: a bbox, replication state, `source`, a foreign writing program.
/// Ten elements per block, so every kind spans blobs. The unsorted variant
/// writes every kind in descending ID order.
fn write_fixture(path: &Path, sorted: bool) {
    let mut hb = HeaderBuilder::new()
        .bbox(
            FIXTURE_BBOX.0,
            FIXTURE_BBOX.1,
            FIXTURE_BBOX.2,
            FIXTURE_BBOX.3,
        )
        .replication_timestamp(REPLICATION_TS)
        .replication_sequence_number(REPLICATION_SEQ)
        .replication_base_url(REPLICATION_URL)
        .source("fixture")
        .writing_program(PROGRAM);
    if sorted {
        hb = hb.sorted();
    }
    let header = hb.build().unwrap();
    let mut writer = PbfWriter::new(
        std::io::BufWriter::new(std::fs::File::create(path).unwrap()),
        Compression::default(),
    );
    writer.write_header(&header).unwrap();

    let order = |n: i64| -> Vec<i64> {
        let ids: Vec<i64> = (1..=n).collect();
        if sorted {
            ids
        } else {
            ids.into_iter().rev().collect()
        }
    };
    let mut bb = BlockBuilder::new();
    let m = meta();
    let flush = |bb: &mut BlockBuilder, writer: &mut PbfWriter<_>| {
        if let Some(bytes) = bb.take().unwrap() {
            writer.write_primitive_block(bytes).unwrap();
        }
    };
    for (i, id) in order(NODES).into_iter().enumerate() {
        if i % 10 == 0 {
            flush(&mut bb, &mut writer);
        }
        let coord = i32::try_from(id * 1000).unwrap();
        let tags: &[(&str, &str)] = if id % 10 == 0 {
            &[("amenity", "bench")]
        } else {
            &[]
        };
        bb.add_node(id, coord, coord, tags.iter().copied(), Some(&m));
    }
    flush(&mut bb, &mut writer);
    for (i, id) in order(WAYS).into_iter().enumerate() {
        if i % 10 == 0 {
            flush(&mut bb, &mut writer);
        }
        // Consecutive ways share two nodes, so the injected pins are set.
        let refs = [id, id + 1, id + 2];
        let tags: &[(&str, &str)] = if id % 2 == 1 {
            &[("highway", "residential")]
        } else {
            &[]
        };
        bb.add_way(id, tags.iter().copied(), &refs, Some(&m));
    }
    flush(&mut bb, &mut writer);
    for id in order(RELATIONS) {
        let members = [
            MemberData {
                id: MemberId::Way(id * 2),
                role: "outer",
            },
            MemberData {
                id: MemberId::Way(id * 2 + 1),
                role: "outer",
            },
        ];
        bb.add_relation(id, [("type", "multipolygon")], &members, Some(&m));
    }
    flush(&mut bb, &mut writer);
    writer.flush().unwrap();
}

fn enrich(input: &Path, output: &Path, prepass: bool) {
    let mut cli = CliInvoker::new()
        .arg("add-locations-to-ways")
        .arg(input)
        .arg("-o")
        .arg(output)
        .arg("--keep-untagged-nodes");
    if prepass {
        cli = cli.args(["--index-type", "external", "--inject-prepass"]);
    }
    cli.assert_success();
}

// ---------------------------------------------------------------------------
// The table
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
enum BboxExp {
    /// The fixture's box, which contains every node of the output.
    Input,
    /// The extract region (`--set-bounds`).
    Region,
    None,
}

/// What a row's output header must say.
#[derive(Clone, Copy, Debug)]
#[allow(clippy::struct_excessive_bools)]
struct Exp {
    sorted: bool,
    low: bool,
    /// Both prepass features: they are declared together or not at all.
    prepass: bool,
    bbox: BboxExp,
    /// `None`: the input's writing program forwarded (degrade's verbatim
    /// header); otherwise the program written.
    program: Option<&'static str>,
    /// The replication sequence number written.
    seq: Option<i64>,
}

impl Exp {
    /// The common case: a re-encoding command that keeps the input's order,
    /// bbox and replication state.
    fn reencoded(f: Fixture) -> Self {
        Self {
            sorted: f.sorted(),
            low: false,
            prepass: false,
            bbox: BboxExp::Input,
            program: Some(OWN_PROGRAM),
            seq: Some(REPLICATION_SEQ),
        }
    }

    /// A command that copies every way blob whole.
    fn copied(f: Fixture) -> Self {
        Self {
            low: f.low(),
            prepass: f.prepass(),
            ..Self::reencoded(f)
        }
    }
}

enum Outcome {
    /// The command refuses this input (an unsorted input to a command that
    /// requires sorted input, a feature it requires and the input lacks).
    Refused,
    Writes(Exp),
}

/// A run's arguments, and every PBF output it writes.
type Invocation = (Vec<OsString>, Vec<PathBuf>);

struct Row {
    command: &'static str,
    label: &'static str,
    /// Arguments, and every PBF output the run writes.
    build: fn(&Fx, Fixture) -> Invocation,
    expect: fn(Fixture) -> Outcome,
}

/// Subcommands that write no PBF header, with the reason.
const NO_PBF_HEADER: &[(&str, &str)] = &[
    ("digest", "prints a digest"),
    ("inspect", "prints a report"),
    ("check", "prints findings"),
    ("bench-read", "reads only"),
    ("bench-write", "encodes into a byte sink, not an output"),
    ("export", "writes GeoJSON"),
    ("diff", "writes text or OSC"),
    ("merge-changes", "writes OSC"),
    ("build-geocode-index", "writes a geocode index directory"),
];

fn words(w: &[&str]) -> Vec<OsString> {
    w.iter().map(OsString::from).collect()
}

/// `command input... -o out` plus extra words.
fn simple(fx: &Fx, command: &str, inputs: &[PathBuf], extra: &[&str]) -> Invocation {
    let out = fx.out(command);
    let mut args = words(&[command]);
    args.extend(inputs.iter().map(OsString::from));
    args.extend(words(extra));
    args.push("-o".into());
    args.push(out.clone().into());
    (args, vec![out])
}

fn region_arg() -> String {
    format!("{},{},{},{}", REGION.0, REGION.1, REGION.2, REGION.3)
}

fn sorted_only(f: Fixture, exp: Exp) -> Outcome {
    if f.sorted() {
        Outcome::Writes(exp)
    } else {
        Outcome::Refused
    }
}

const ROWS: &[Row] = &[
    Row {
        command: "cat",
        label: "cat",
        build: |fx, f| simple(fx, "cat", &[fx.path(f)], &[]),
        expect: |f| Outcome::Writes(Exp::copied(f)),
    },
    Row {
        command: "cat",
        label: "cat --type way",
        build: |fx, f| simple(fx, "cat", &[fx.path(f)], &["-t", "way"]),
        expect: |f| Outcome::Writes(Exp::copied(f)),
    },
    Row {
        command: "cat",
        label: "cat -c version",
        build: |fx, f| simple(fx, "cat", &[fx.path(f)], &["--clean", "version"]),
        expect: |f| Outcome::Writes(Exp::reencoded(f)),
    },
    Row {
        command: "cat",
        label: "cat of two inputs",
        build: |fx, f| simple(fx, "cat", &[fx.path(f), fx.path(f)], &[]),
        // A concatenation is not sorted; the prepass features are global
        // facts a second input can falsify, LocationsOnWays is per way.
        expect: |f| {
            Outcome::Writes(Exp {
                sorted: false,
                prepass: false,
                ..Exp::copied(f)
            })
        },
    },
    Row {
        command: "cat",
        label: "cat --dedupe of two inputs",
        build: |fx, f| simple(fx, "cat", &[fx.path(f), fx.path(f)], &["--dedupe"]),
        // Every blob overlaps its twin, so every way is re-encoded.
        expect: |f| {
            sorted_only(
                f,
                Exp {
                    sorted: true,
                    ..Exp::reencoded(f)
                },
            )
        },
    },
    Row {
        command: "sort",
        label: "sort",
        build: |fx, f| simple(fx, "sort", &[fx.path(f)], &[]),
        // Sorted input passes through blob by blob; the unsorted fixture
        // carries no enrichment for the rewrite to drop.
        expect: |f| {
            Outcome::Writes(Exp {
                sorted: true,
                ..Exp::copied(f)
            })
        },
    },
    Row {
        command: "repack",
        label: "repack",
        build: |fx, f| simple(fx, "repack", &[fx.path(f)], &[]),
        expect: |f| {
            Outcome::Writes(Exp {
                low: f.low(),
                ..Exp::reencoded(f)
            })
        },
    },
    Row {
        command: "degrade",
        label: "degrade --strip-indexdata",
        build: |fx, f| simple(fx, "degrade", &[fx.path(f)], &["--strip-indexdata"]),
        expect: |f| {
            Outcome::Writes(Exp {
                program: None,
                ..Exp::copied(f)
            })
        },
    },
    Row {
        command: "degrade",
        label: "degrade --strip-tagdata --generator --output-header",
        build: |fx, f| {
            simple(
                fx,
                "degrade",
                &[fx.path(f)],
                &[
                    "--strip-tagdata",
                    "--generator",
                    "gen",
                    "--output-header",
                    "osmosis_replication_sequence_number=9",
                ],
            )
        },
        // An override replaces its own field and nothing else.
        expect: |f| {
            Outcome::Writes(Exp {
                program: Some("gen"),
                seq: Some(9),
                ..Exp::copied(f)
            })
        },
    },
    Row {
        command: "degrade",
        label: "degrade --strip-bbox",
        build: |fx, f| simple(fx, "degrade", &[fx.path(f)], &["--strip-bbox"]),
        expect: |f| {
            Outcome::Writes(Exp {
                program: None,
                bbox: BboxExp::None,
                ..Exp::copied(f)
            })
        },
    },
    Row {
        command: "degrade",
        label: "degrade --drop-ids",
        build: |fx, f| simple(fx, "degrade", &[fx.path(f)], &["--drop-ids", "1:1"]),
        expect: |f| Outcome::Writes(Exp::reencoded(f)),
    },
    Row {
        command: "degrade",
        label: "degrade --unsort",
        build: |fx, f| {
            simple(
                fx,
                "degrade",
                &[fx.path(f)],
                &["--unsort", "--block-cap", "4"],
            )
        },
        expect: |f| {
            Outcome::Writes(Exp {
                sorted: false,
                ..Exp::reencoded(f)
            })
        },
    },
    Row {
        command: "renumber",
        label: "renumber",
        build: |fx, f| simple(fx, "renumber", &[fx.path(f)], &[]),
        expect: |f| sorted_only(f, Exp::reencoded(f)),
    },
    Row {
        command: "tags-filter",
        label: "tags-filter",
        build: |fx, f| simple(fx, "tags-filter", &[fx.path(f)], &["highway"]),
        expect: |f| Outcome::Writes(Exp::reencoded(f)),
    },
    Row {
        command: "tags-filter",
        label: "tags-filter -R",
        build: |fx, f| simple(fx, "tags-filter", &[fx.path(f)], &["-R", "highway"]),
        expect: |f| Outcome::Writes(Exp::reencoded(f)),
    },
    Row {
        command: "getid",
        label: "getid",
        build: |fx, f| simple(fx, "getid", &[fx.path(f)], &["n10", "w3"]),
        expect: |f| Outcome::Writes(Exp::reencoded(f)),
    },
    Row {
        command: "getid",
        label: "getid -r",
        build: |fx, f| simple(fx, "getid", &[fx.path(f)], &["-r", "w3"]),
        expect: |f| Outcome::Writes(Exp::reencoded(f)),
    },
    Row {
        command: "getid",
        label: "getid --invert of a node",
        build: |fx, f| simple(fx, "getid", &[fx.path(f)], &["--invert", "n10"]),
        // No way is removed, so every way blob is copied whole.
        expect: |f| Outcome::Writes(Exp::copied(f)),
    },
    Row {
        command: "getid",
        label: "getid --invert of a way",
        build: |fx, f| simple(fx, "getid", &[fx.path(f)], &["--invert", "w3"]),
        expect: |f| Outcome::Writes(Exp::reencoded(f)),
    },
    Row {
        command: "getparents",
        label: "getparents",
        build: |fx, f| simple(fx, "getparents", &[fx.path(f)], &["n4"]),
        expect: |f| Outcome::Writes(Exp::reencoded(f)),
    },
    Row {
        command: "extract",
        label: "extract --simple",
        build: |fx, f| {
            simple(
                fx,
                "extract",
                &[fx.path(f)],
                &["--simple", "-b", &region_arg()],
            )
        },
        expect: |f| Outcome::Writes(Exp::reencoded(f)),
    },
    Row {
        command: "extract",
        label: "extract (complete_ways)",
        build: |fx, f| simple(fx, "extract", &[fx.path(f)], &["-b", &region_arg()]),
        expect: |f| Outcome::Writes(Exp::reencoded(f)),
    },
    Row {
        command: "extract",
        label: "extract --smart",
        build: |fx, f| {
            simple(
                fx,
                "extract",
                &[fx.path(f)],
                &["--smart", "-b", &region_arg()],
            )
        },
        expect: |f| Outcome::Writes(Exp::reencoded(f)),
    },
    Row {
        command: "extract",
        label: "extract --simple --set-bounds",
        build: |fx, f| {
            simple(
                fx,
                "extract",
                &[fx.path(f)],
                &["--simple", "--set-bounds", "-b", &region_arg()],
            )
        },
        expect: |f| {
            Outcome::Writes(Exp {
                bbox: BboxExp::Region,
                ..Exp::reencoded(f)
            })
        },
    },
    Row {
        command: "extract",
        label: "extract --smart --set-bounds",
        build: |fx, f| {
            simple(
                fx,
                "extract",
                &[fx.path(f)],
                &["--smart", "--set-bounds", "-b", &region_arg()],
            )
        },
        expect: |f| {
            Outcome::Writes(Exp {
                bbox: BboxExp::Region,
                ..Exp::reencoded(f)
            })
        },
    },
    Row {
        command: "extract",
        label: "extract --config --simple",
        build: extract_config,
        expect: |f| Outcome::Writes(Exp::reencoded(f)),
    },
    Row {
        command: "add-locations-to-ways",
        label: "add-locations-to-ways",
        build: |fx, f| {
            simple(
                fx,
                "add-locations-to-ways",
                &[fx.path(f)],
                &["--keep-untagged-nodes"],
            )
        },
        expect: |f| {
            Outcome::Writes(Exp {
                low: true,
                ..Exp::reencoded(f)
            })
        },
    },
    Row {
        command: "add-locations-to-ways",
        label: "add-locations-to-ways --inject-prepass",
        build: |fx, f| {
            simple(
                fx,
                "add-locations-to-ways",
                &[fx.path(f)],
                &[
                    "--keep-untagged-nodes",
                    "--index-type",
                    "external",
                    "--inject-prepass",
                ],
            )
        },
        expect: |f| {
            sorted_only(
                f,
                Exp {
                    low: true,
                    prepass: true,
                    ..Exp::reencoded(f)
                },
            )
        },
    },
    Row {
        command: "time-filter",
        label: "time-filter",
        build: |fx, f| simple(fx, "time-filter", &[fx.path(f)], &["4102444800"]),
        expect: |f| sorted_only(f, Exp::reencoded(f)),
    },
    Row {
        command: "apply-changes",
        label: "apply-changes",
        build: |fx, f| simple(fx, "apply-changes", &[fx.path(f), fx.osc()], &[]),
        // The OSC creates a node outside the base's box.
        expect: |f| {
            sorted_only(
                f,
                Exp {
                    sorted: true,
                    bbox: BboxExp::None,
                    ..Exp::reencoded(f)
                },
            )
        },
    },
    Row {
        command: "apply-changes",
        label: "apply-changes -l",
        build: |fx, f| {
            simple(
                fx,
                "apply-changes",
                &[fx.path(f), fx.osc()],
                &["--locations-on-ways"],
            )
        },
        expect: |f| {
            if f.low() {
                Outcome::Writes(Exp {
                    sorted: true,
                    low: true,
                    bbox: BboxExp::None,
                    ..Exp::reencoded(f)
                })
            } else {
                Outcome::Refused
            }
        },
    },
    Row {
        command: "bench-merge",
        label: "bench-merge",
        build: |fx, f| simple(fx, "bench-merge", &[fx.path(f), fx.osc()], &[]),
        expect: |f| {
            sorted_only(
                f,
                Exp {
                    sorted: true,
                    bbox: BboxExp::None,
                    ..Exp::reencoded(f)
                },
            )
        },
    },
];

/// Two config-driven outputs, so the multi-extract path (single pass on
/// sorted input, one extract per region otherwise) is covered.
fn extract_config(fx: &Fx, f: Fixture) -> Invocation {
    let n = fx.out("config");
    let dir = n.with_extension("d");
    std::fs::create_dir(&dir).unwrap();
    let config = dir.join("config.json");
    let region = format!("[{},{},{},{}]", REGION.0, REGION.1, REGION.2, REGION.3);
    std::fs::write(
        &config,
        format!(
            r#"{{"directory":"{}","extracts":[
                {{"output":"a.osm.pbf","bbox":{region}}},
                {{"output":"b.osm.pbf","bbox":{region}}}]}}"#,
            dir.display()
        ),
    )
    .unwrap();
    let mut args = words(&["extract"]);
    args.push(fx.path(f).into());
    args.extend(words(&["--simple", "--config"]));
    args.push(config.into());
    (args, vec![dir.join("a.osm.pbf"), dir.join("b.osm.pbf")])
}

// ---------------------------------------------------------------------------
// Reading an output back
// ---------------------------------------------------------------------------

/// A way blob's field 5: the membership bitmap and the way count it encodes.
type Field5 = (Vec<u8>, u32);

/// What an output actually contains, for the soundness checks.
struct Contents {
    /// Every element follows its predecessor in type-then-ID order.
    sorted: bool,
    ways: usize,
    /// Ways with one location per ref.
    ways_located: usize,
    way_blobs: usize,
    way_blobs_with_field5: usize,
    /// Way ID to its SharedNodePins bitmap.
    pins: HashMap<i64, Option<Vec<u8>>>,
    /// Every way's ID and refs, in file order.
    way_refs: Vec<(i64, Vec<i64>)>,
    /// Per way blob: its way IDs in order, and its field 5 as (bitmap, way
    /// count) when present.
    way_blob_members: Vec<(Vec<i64>, Option<Field5>)>,
    /// Ways that are members of a multipolygon or boundary relation.
    member_ways: std::collections::HashSet<i64>,
    /// Node IDs.
    node_ids: std::collections::HashSet<i64>,
    /// Node coordinates (lon, lat) in nanodegrees.
    nodes: Vec<(i64, i64)>,
}

fn bit(bits: &[u8], i: usize) -> bool {
    bits.get(i / 8).is_some_and(|b| b & (1 << (i % 8)) != 0)
}

/// The pin bits the output's own ways require (ADR-0007): a position is
/// pinned when its node occurs at two or more non-closure positions across
/// the file's ways and resolves to a node in the file. A pin beyond these is
/// allowed (a subset only over-pins); a missing one is a false declaration.
fn missing_pins(c: &Contents) -> Vec<(i64, usize)> {
    let trimmed = |refs: &[i64]| -> usize {
        if refs.len() >= 4 && refs.first() == refs.last() {
            refs.len() - 1
        } else {
            refs.len()
        }
    };
    let mut count: HashMap<i64, u32> = HashMap::new();
    for (_, refs) in &c.way_refs {
        for &id in &refs[..trimmed(refs)] {
            *count.entry(id).or_default() += 1;
        }
    }
    let mut missing = Vec::new();
    for (id, refs) in &c.way_refs {
        let pins = c.pins.get(id).cloned().flatten().unwrap_or_default();
        for (i, node) in refs.iter().enumerate() {
            let required = count.get(node).copied().unwrap_or(0) >= 2 && c.node_ids.contains(node);
            if required && !bit(&pins, i) {
                missing.push((*id, i));
            }
        }
    }
    missing
}

/// Way blobs whose field 5 is absent, counts a different number of ways than
/// the blob holds, or leaves a multipolygon or boundary member's bit clear.
fn bad_way_member_blobs(c: &Contents) -> Vec<String> {
    let mut bad = Vec::new();
    for (ways, field5) in &c.way_blob_members {
        match field5 {
            None => bad.push(format!("blob of ways {ways:?} has no field 5")),
            Some((bits, count)) => {
                if *count as usize != ways.len() {
                    bad.push(format!(
                        "field 5 counts {count} ways, the blob holds {}",
                        ways.len()
                    ));
                }
                for (i, id) in ways.iter().enumerate() {
                    if c.member_ways.contains(id) && !bit(bits, i) {
                        bad.push(format!("member way {id} has its field-5 bit clear"));
                    }
                }
            }
        }
    }
    bad
}

fn kind_rank(e: &Element<'_>) -> u8 {
    match e {
        Element::Node(_) | Element::DenseNode(_) => 0,
        Element::Way(_) => 1,
        _ => 2,
    }
}

fn read_contents(path: &Path) -> Contents {
    let mut reader = BlobReader::from_path(path).unwrap();
    reader.set_parse_waymembers(true);
    let mut c = Contents {
        sorted: true,
        ways: 0,
        ways_located: 0,
        way_blobs: 0,
        way_blobs_with_field5: 0,
        pins: HashMap::new(),
        way_refs: Vec::new(),
        way_blob_members: Vec::new(),
        member_ways: std::collections::HashSet::new(),
        node_ids: std::collections::HashSet::new(),
        nodes: Vec::new(),
    };
    let mut last: Option<(u8, i64)> = None;
    for blob in reader {
        let blob = blob.unwrap();
        let field5 = blob
            .way_members()
            .map(|bits| (bits.to_vec(), blob.way_member_count().unwrap()));
        let has_field5 = field5.is_some();
        let BlobDecode::OsmData(block) = blob.decode().unwrap() else {
            continue;
        };
        let mut has_way = false;
        let mut blob_ways = Vec::new();
        for element in block.elements() {
            let key = (kind_rank(&element), element_id(&element));
            if last.is_some_and(|l| key <= l) {
                c.sorted = false;
            }
            last = Some(key);
            match element {
                Element::Way(w) => {
                    has_way = true;
                    c.ways += 1;
                    let refs = w.refs().count();
                    if refs > 0 && w.node_locations().count() == refs {
                        c.ways_located += 1;
                    }
                    c.pins
                        .insert(w.id(), w.shared_node_pins().map(<[u8]>::to_vec));
                    c.way_refs.push((w.id(), w.refs().collect()));
                    blob_ways.push(w.id());
                }
                Element::Node(n) => {
                    c.node_ids.insert(n.id());
                    c.nodes.push((n.nano_lon(), n.nano_lat()));
                }
                Element::DenseNode(n) => {
                    c.node_ids.insert(n.id());
                    c.nodes.push((n.nano_lon(), n.nano_lat()));
                }
                Element::Relation(r) => {
                    let area = r
                        .tags()
                        .any(|(k, v)| k == "type" && matches!(v, "multipolygon" | "boundary"));
                    if area {
                        for m in r.members() {
                            if let MemberId::Way(id) = m.id {
                                c.member_ways.insert(id);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        if has_way {
            c.way_blobs += 1;
            if has_field5 {
                c.way_blobs_with_field5 += 1;
            }
            c.way_blob_members.push((blob_ways, field5));
        }
    }
    c
}

fn element_id(e: &Element<'_>) -> i64 {
    match e {
        Element::Node(n) => n.id(),
        Element::DenseNode(n) => n.id(),
        Element::Way(w) => w.id(),
        Element::Relation(r) => r.id(),
        _ => unreachable!("an element kind this test does not know"),
    }
}

fn read_header(path: &Path) -> HeaderBlock {
    crate::common::read_header(path)
}

/// Nanodegrees of a header bbox edge.
#[allow(clippy::cast_possible_truncation)]
fn nanos(deg: f64) -> i64 {
    (deg * 1e9).round() as i64
}

// ---------------------------------------------------------------------------
// The checks
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_lines)]
fn check_output(
    path: &Path,
    exp: &Exp,
    fixture: Fixture,
    source_pins: &HashMap<i64, Option<Vec<u8>>>,
) -> Result<(), String> {
    let h = read_header(path);
    let c = read_contents(path);

    // Exact declarations.
    let got = (
        h.is_sorted(),
        h.has_locations_on_ways(),
        h.has_way_members_v1(),
        h.has_shared_node_pins_v1(),
    );
    let want = (exp.sorted, exp.low, exp.prepass, exp.prepass);
    ensure!(
        got == want,
        "declares (sorted, LocationsOnWays, WayMembers-v1, SharedNodePins-v1) = {got:?}, \
         expected {want:?}"
    );

    // Soundness: every declaration holds for the content. Every row writes
    // ways, so the coverage checks below are never satisfied vacuously.
    ensure!(c.ways > 0, "the output holds no ways");
    ensure!(
        !h.is_sorted() || c.sorted,
        "declares Sort.Type_then_ID over unsorted content"
    );
    ensure!(
        !h.has_locations_on_ways() || c.ways_located == c.ways,
        "declares LocationsOnWays but {} of {} ways carry no locations",
        c.ways - c.ways_located,
        c.ways
    );
    if h.has_way_members_v1() {
        let bad = bad_way_member_blobs(&c);
        ensure!(bad.is_empty(), "declares WayMembers-v1 but: {bad:?}");
    }
    if h.has_shared_node_pins_v1() {
        let missing = missing_pins(&c);
        ensure!(
            missing.is_empty(),
            "declares SharedNodePins-v1 but (way, position) {missing:?} is shared and unpinned"
        );
    }
    // Copied pins are the input's, byte for byte.
    if h.has_shared_node_pins_v1() && fixture.prepass() {
        for (id, pins) in &c.pins {
            ensure!(
                source_pins.get(id) == Some(pins),
                "declares SharedNodePins-v1 but way {id} carries {pins:?}, the input {:?}",
                source_pins.get(id)
            );
        }
    }

    // Header fields.
    match exp.bbox {
        BboxExp::Input => {
            let b = h.bbox().ok_or("the input's bbox was dropped")?;
            ensure!(
                (nanos(b.left), nanos(b.bottom), nanos(b.right), nanos(b.top))
                    == (
                        nanos(FIXTURE_BBOX.0),
                        nanos(FIXTURE_BBOX.1),
                        nanos(FIXTURE_BBOX.2),
                        nanos(FIXTURE_BBOX.3)
                    ),
                "bbox ({}, {}, {}, {}) is not the input's",
                b.left,
                b.bottom,
                b.right,
                b.top
            );
            for &(lon, lat) in &c.nodes {
                ensure!(
                    (nanos(b.left)..=nanos(b.right)).contains(&lon)
                        && (nanos(b.bottom)..=nanos(b.top)).contains(&lat),
                    "node at ({lon}, {lat}) nanodegrees lies outside the declared bbox"
                );
            }
        }
        BboxExp::Region => {
            let b = h.bbox().ok_or("--set-bounds wrote no bbox")?;
            ensure!(
                (nanos(b.left), nanos(b.bottom), nanos(b.right), nanos(b.top))
                    == (
                        nanos(REGION.0),
                        nanos(REGION.1),
                        nanos(REGION.2),
                        nanos(REGION.3)
                    ),
                "bbox is not the region"
            );
        }
        BboxExp::None => ensure!(h.bbox().is_none(), "declares a bbox it cannot vouch for"),
    }
    let program = exp.program.unwrap_or(fixture.program());
    ensure!(
        h.writing_program() == Some(program),
        "writing program {:?}, expected {program:?}",
        h.writing_program()
    );
    ensure!(
        h.osmosis_replication_timestamp() == Some(REPLICATION_TS)
            && h.osmosis_replication_sequence_number() == exp.seq
            && h.osmosis_replication_base_url() == Some(REPLICATION_URL),
        "replication state ({:?}, {:?}, {:?}), expected ({REPLICATION_TS}, {:?}, {REPLICATION_URL})",
        h.osmosis_replication_timestamp(),
        h.osmosis_replication_sequence_number(),
        h.osmosis_replication_base_url(),
        exp.seq
    );
    Ok(())
}

fn run_row(
    fx: &Fx,
    row: &Row,
    fixture: Fixture,
    pins: &HashMap<i64, Option<Vec<u8>>>,
) -> Result<(), String> {
    let (args, outputs) = (row.build)(fx, fixture);
    let out: CliOutput = CliInvoker::new().args(&args).run();
    match (row.expect)(fixture) {
        Outcome::Refused => {
            ensure!(
                !out.status.success(),
                "expected a refusal, the run succeeded"
            );
            Ok(())
        }
        Outcome::Writes(exp) => {
            ensure!(
                out.status.success(),
                "exit {:?}; stderr:\n{}",
                out.status.code(),
                out.stderr_str()
            );
            for output in &outputs {
                check_output(output, &exp, fixture, pins)
                    .map_err(|e| format!("{}: {e}", output.display()))?;
            }
            Ok(())
        }
    }
}

fn run_fixture(fixture: Fixture) {
    let fx = Fx::new(&[fixture]);
    let pins = if fixture.prepass() {
        read_contents(&fx.path(fixture)).pins
    } else {
        HashMap::new()
    };
    let mut failures = Vec::new();
    for row in ROWS {
        if let Err(why) = run_row(&fx, row, fixture, &pins) {
            failures.push(format!("[{}] {why}", row.label));
        }
    }
    assert!(
        failures.is_empty(),
        "{fixture:?} input:\n\n{}",
        failures.join("\n\n")
    );
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn every_subcommand_is_in_the_table_or_writes_no_pbf_header() {
    let commands = subcommands();
    assert!(
        commands.len() >= 20,
        "the clap tree lists only {commands:?}"
    );
    let mut problems = Vec::new();
    for command in &commands {
        let in_table = ROWS.iter().any(|row| row.command == command);
        let no_header = NO_PBF_HEADER.iter().any(|(name, _)| name == command);
        match (in_table, no_header) {
            (false, false) => problems.push(format!(
                "`{command}` is in neither ROWS nor NO_PBF_HEADER: add a row for it, \
                 or say why it writes no PBF header"
            )),
            (true, true) => problems.push(format!("`{command}` is in both")),
            _ => {}
        }
    }
    for name in ROWS
        .iter()
        .map(|row| row.command)
        .chain(NO_PBF_HEADER.iter().map(|(name, _)| *name))
    {
        if !commands.iter().any(|c| c == name) {
            problems.push(format!(
                "`{name}` is in the table but the binary has no such subcommand"
            ));
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn every_command_declares_what_a_plain_sorted_input_yields() {
    run_fixture(Fixture::Plain);
}

#[test]
fn every_command_declares_what_an_unsorted_input_yields() {
    run_fixture(Fixture::Unsorted);
}

#[test]
fn every_command_declares_what_a_locations_on_ways_input_yields() {
    run_fixture(Fixture::Low);
}

#[test]
fn every_command_declares_what_an_injected_prepass_input_yields() {
    run_fixture(Fixture::Prepass);
}

/// Extract stamped `Sort.Type_then_ID` on every output, whatever the input
/// declared. On unsorted input every strategy keeps the input's order, so the
/// output must not claim sorted - single region and the per-region fallback
/// of a config alike.
#[test]
fn an_extract_of_unsorted_input_does_not_declare_sorted() {
    let fx = Fx::new(&[Fixture::Unsorted]);
    let region = region_arg();
    let mut runs: Vec<Invocation> = [
        &["--simple", "-b", region.as_str()][..],
        &["-b", region.as_str()][..],
        &["--smart", "-b", region.as_str()][..],
    ]
    .iter()
    .map(|extra| simple(&fx, "extract", &[fx.path(Fixture::Unsorted)], extra))
    .collect();
    runs.push(extract_config(&fx, Fixture::Unsorted));
    for (args, outputs) in runs {
        CliInvoker::new().args(&args).assert_success();
        for output in outputs {
            assert!(
                !read_header(&output).is_sorted(),
                "{args:?}: {} declares Sort.Type_then_ID over unsorted input",
                output.display()
            );
            assert!(
                !read_contents(&output).sorted,
                "fixture precondition: content is unsorted"
            );
        }
    }
}

/// Commands that copy way blobs whole used to drop LocationsOnWays and the
/// prepass declarations from the header while copying the blobs that carry
/// them, and degrade dropped the prepass declarations once given
/// `--generator`. Each now keeps every declaration its blobs back.
#[test]
fn raw_copying_commands_keep_the_enrichment_their_blobs_carry() {
    let fx = Fx::new(&[Fixture::Prepass]);
    let input = fx.path(Fixture::Prepass);
    let pins = read_contents(&input).pins;
    let runs = [
        simple(&fx, "cat", std::slice::from_ref(&input), &[]),
        simple(&fx, "cat", std::slice::from_ref(&input), &["-t", "way"]),
        simple(&fx, "sort", std::slice::from_ref(&input), &[]),
        simple(
            &fx,
            "getid",
            std::slice::from_ref(&input),
            &["--invert", "n10"],
        ),
        simple(
            &fx,
            "degrade",
            std::slice::from_ref(&input),
            &["--strip-indexdata", "--generator", "gen"],
        ),
    ];
    for (args, outputs) in runs {
        CliInvoker::new().args(&args).assert_success();
        let h = read_header(&outputs[0]);
        assert!(
            h.has_locations_on_ways() && h.has_way_members_v1() && h.has_shared_node_pins_v1(),
            "{args:?}: the output's way blobs carry all three enrichments, its header \
             declares {:?}",
            h.optional_features()
        );
        let c = read_contents(&outputs[0]);
        assert_eq!(
            c.ways,
            usize::try_from(WAYS).unwrap(),
            "{args:?}: every way reaches the output"
        );
        assert_eq!(c.ways_located, c.ways, "{args:?}");
        assert!(bad_way_member_blobs(&c).is_empty(), "{args:?}");
        assert!(missing_pins(&c).is_empty(), "{args:?}");
        assert_eq!(c.way_blobs_with_field5, c.way_blobs, "{args:?}");
        for (id, p) in &c.pins {
            assert_eq!(pins.get(id), Some(p), "{args:?}: way {id}");
        }
    }
}

/// The raw-copying paths decide their header from the first data blob's
/// indexdata. A later way blob without indexdata would go through a branch
/// that drops what the header already promised, so the run refuses it
/// instead of writing a header that lies.
#[test]
fn an_unindexed_way_blob_cannot_break_a_declared_enrichment() {
    let fx = Fx::new(&[Fixture::Prepass]);
    let input = fx.path(Fixture::Prepass);
    let bytes = std::fs::read(&input).unwrap();
    let way_blob = BlobReader::from_path(&input)
        .unwrap()
        .position(|blob| {
            matches!(
                blob.unwrap().decode().unwrap(),
                BlobDecode::OsmData(block) if block.elements().any(|e| matches!(e, Element::Way(_)))
            )
        })
        .unwrap();
    let mixed = fx.dir.path().join("mixed.osm.pbf");
    std::fs::write(
        &mixed,
        crate::common::adversarial::mutate_blob_header_indexdata(&bytes, way_blob, Vec::clear),
    )
    .unwrap();
    for extra in [&[][..], &["-t", "way"][..]] {
        let (args, outputs) = simple(&fx, "cat", std::slice::from_ref(&mixed), extra);
        let out = CliInvoker::new().args(&args).run();
        assert!(
            !out.status.success(),
            "cat {extra:?} wrote a way blob without the enrichment its header declared"
        );
        assert!(
            out.stderr_str().contains("without indexdata"),
            "stderr:\n{}",
            out.stderr_str()
        );
        assert!(!outputs[0].exists());
    }
}

/// A cutoff before the input's replication state lowers the timestamp and
/// drops the sequence number, which named a later state than the data.
#[test]
fn time_filter_before_the_input_state_drops_the_sequence_number() {
    let fx = Fx::new(&[Fixture::Plain]);
    let (args, outputs) = simple(&fx, "time-filter", &[fx.path(Fixture::Plain)], &["500"]);
    CliInvoker::new().args(&args).assert_success();
    let h = read_header(&outputs[0]);
    assert_eq!(h.osmosis_replication_timestamp(), Some(500));
    assert_eq!(h.osmosis_replication_sequence_number(), None);
    assert_eq!(h.osmosis_replication_base_url(), Some(REPLICATION_URL));
}

/// `--output-header osmosis_replication_timestamp` takes the same two
/// spellings as time-filter's cutoff.
#[test]
fn the_replication_timestamp_override_accepts_rfc3339() {
    let fx = Fx::new(&[Fixture::Plain]);
    let (args, outputs) = simple(
        &fx,
        "cat",
        &[fx.path(Fixture::Plain)],
        &[
            "--output-header",
            "osmosis_replication_timestamp=1970-01-01T00:33:20Z",
        ],
    );
    CliInvoker::new().args(&args).assert_success();
    assert_eq!(
        read_header(&outputs[0]).osmosis_replication_timestamp(),
        Some(2000),
        "the fixture's own timestamp is {REPLICATION_TS}; the override must replace it"
    );
}

/// OSC output has no PBF header, so the header flags are refused rather than
/// silently ignored.
#[test]
fn tags_filter_osc_mode_refuses_header_overrides() {
    let fx = Fx::new(&[Fixture::Plain]);
    let out = fx.dir.path().join("out.osc");
    for flags in [
        &["--generator", "gen"][..],
        &["--output-header", "osmosis_replication_sequence_number=1"][..],
    ] {
        let run = CliInvoker::new()
            .arg("tags-filter")
            .arg(fx.osc())
            .args(["--input-kind", "osc", "amenity", "-o"])
            .arg(&out)
            .args(flags)
            .run();
        assert!(!run.status.success(), "{flags:?} was accepted in OSC mode");
        assert!(run.stderr_str().contains("not valid in OSC mode"));
    }
}
