//! degrade's library surface where the command line cannot reach it: a
//! `Reencode` with none of its transformations set, and the block cap that
//! only a re-encode carries.
//!
//! The command line asks for a re-encode only through a decode-path flag,
//! so an empty `Reencode` is the library's alone. It is the decode route
//! all the same: the indexdata precondition, ways written without inline
//! coordinates, and the warning a re-encode owes a `LocationsOnWays` input
//! unless `strip_locations` declares the loss intended.

use std::path::Path;
use std::sync::Arc;

use pbfhogg::block_builder::{BlockBuilder, HeaderBuilder};
use pbfhogg::degrade::{DegradeFlags, Reencode};
use pbfhogg::write::WriteOptions;
use pbfhogg::writer::{Compression, PbfWriter};

/// Nodes 1..=10 and three ways with inline coordinates, under a sorted
/// header declaring `LocationsOnWays`; indexed when `indexed`.
fn write_located(path: &Path, indexed: bool) {
    let header = HeaderBuilder::new()
        .sorted()
        .optional_feature("LocationsOnWays")
        .build()
        .expect("build header");
    let file = std::fs::File::create(path).expect("create fixture");
    let mut writer = PbfWriter::new(std::io::BufWriter::new(file), Compression::default());
    writer.write_header(&header).expect("write header");
    let write = |bb: &mut BlockBuilder, writer: &mut PbfWriter<_>| {
        let bytes = bb.take().expect("take").expect("a block");
        if indexed {
            writer.write_primitive_block(bytes).expect("write block");
        } else {
            writer
                .write_primitive_block_no_indexdata(bytes)
                .expect("write block");
        }
    };
    let mut bb = BlockBuilder::new();
    for id in 1..=10_i64 {
        let at = i32::try_from(id).expect("small") * 1_000;
        bb.add_node(id, at, at, [("n", "v")], None);
    }
    write(&mut bb, &mut writer);
    for id in 1..=3_i64 {
        let refs = [id, id + 1, id + 2];
        let locations: Vec<(i32, i32)> = refs
            .iter()
            .map(|r| {
                let at = i32::try_from(*r).expect("small") * 1_000;
                (at, at)
            })
            .collect();
        bb.add_way_with_locations(id, [("w", "v")], &refs, &locations, None);
    }
    write(&mut bb, &mut writer);
    writer.flush().expect("flush fixture");
}

/// Records the key of every diagnostic the run emits.
#[derive(Default)]
struct Keys(std::sync::Mutex<Vec<String>>);
impl pbfhogg::host::DiagnosticSink for Keys {
    fn event(&self, event: &pbfhogg::host::Event<'_>) {
        self.0.lock().expect("sink").push(event.key.to_owned());
    }
}

/// Records every counter the run emits.
#[derive(Default)]
struct Counters(std::sync::Mutex<Vec<(String, i64)>>);
impl pbfhogg::debug::InstrumentSink for Counters {
    fn marker(&self, _name: &str) {}
    fn counter(&self, name: &str, value: i64) {
        self.0.lock().expect("sink").push((name.to_owned(), value));
    }
}

struct Run {
    result: pbfhogg::Result<pbfhogg::degrade::DegradeStats>,
    diagnostics: Vec<String>,
    counters: Vec<(String, i64)>,
}

fn run(input: &Path, output: &Path, flags: DegradeFlags, force: bool) -> Run {
    let keys = Arc::new(Keys::default());
    let counters = Arc::new(Counters::default());
    let mut host = crate::common::test_host();
    host.diagnostics = pbfhogg::host::Diagnostics::new(
        Arc::clone(&keys) as Arc<dyn pbfhogg::host::DiagnosticSink>
    );
    host.invocation = pbfhogg::host::Invocation::with_sink(
        Arc::clone(&counters) as Arc<dyn pbfhogg::debug::InstrumentSink>
    );
    let result = pbfhogg::degrade::degrade(
        input,
        crate::common::out_replace(output),
        flags,
        &WriteOptions {
            force,
            ..WriteOptions::default()
        },
        &pbfhogg::HeaderOverrides::default(),
        &host,
    );
    let diagnostics = keys.0.lock().expect("sink").clone();
    let counters = counters.0.lock().expect("sink").clone();
    Run {
        result,
        diagnostics,
        counters,
    }
}

fn reencode(reencode: Reencode) -> DegradeFlags {
    DegradeFlags {
        reencode: Some(reencode),
        ..DegradeFlags::default()
    }
}

const LOSS: &str = "header.way_metadata_loss";

/// `(kind, id)` of every element in `path`, sorted, and how many ways carry
/// inline coordinates.
fn elements(path: &Path) -> (Vec<(u8, i64)>, usize) {
    let mut ids = Vec::new();
    let mut located = 0;
    for blob in pbfhogg::BlobReader::from_path(path).expect("open") {
        let pbfhogg::BlobDecode::OsmData(block) = blob.expect("blob").decode().expect("decode")
        else {
            continue;
        };
        for element in block.elements() {
            match element {
                pbfhogg::Element::DenseNode(n) => ids.push((0, n.id())),
                pbfhogg::Element::Node(n) => ids.push((0, n.id())),
                pbfhogg::Element::Way(w) => {
                    ids.push((1, w.id()));
                    located += usize::from(w.node_locations().next().is_some());
                }
                pbfhogg::Element::Relation(r) => ids.push((2, r.id())),
                _ => {}
            }
        }
    }
    ids.sort_unstable();
    (ids, located)
}

/// An empty `Reencode` re-encodes: every element survives, the ways lose
/// their inline coordinates and the header its `LocationsOnWays`, with the
/// loss warned about; the summary names the operation.
#[test]
fn an_empty_reencode_reencodes_and_warns_of_the_lost_locations() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");
    write_located(&input, true);

    let (input_ids, input_located) = elements(&input);
    assert_eq!(input_located, 3, "the fixture's ways carry coordinates");

    let done = run(&input, &output, reencode(Reencode::default()), false);
    let stats = done.result.expect("an empty reencode runs");
    assert_eq!(stats.elements_written, 13);
    let (output_ids, output_located) = elements(&output);
    assert_eq!(output_ids, input_ids, "every element survives");
    assert_eq!(
        output_located, 0,
        "a re-encode writes ways without inline coordinates"
    );
    assert!(!crate::common::read_header(&output).has_locations_on_ways());
    assert!(
        done.diagnostics.iter().any(|k| k == LOSS),
        "the lost LocationsOnWays is warned about: {:?}",
        done.diagnostics
    );
    let mut summary = Vec::new();
    stats.write_summary(&mut summary).expect("summary");
    let summary = String::from_utf8(summary).expect("utf-8");
    assert!(summary.contains("(applied: reencode)"), "{summary}");
}

/// `strip_locations` asks for the loss, so it is not warned about.
#[test]
fn strip_locations_declares_the_loss_intended() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");
    write_located(&input, true);

    let done = run(
        &input,
        &output,
        reencode(Reencode {
            strip_locations: true,
            ..Reencode::default()
        }),
        false,
    );
    done.result.expect("strip_locations runs");
    assert!(!crate::common::read_header(&output).has_locations_on_ways());
    assert!(
        !done.diagnostics.iter().any(|k| k == LOSS),
        "{:?}",
        done.diagnostics
    );
}

/// An empty `Reencode` is the decode route, so it refuses unindexed input
/// unless forced, as every re-encode does.
#[test]
fn an_empty_reencode_needs_indexdata_unless_forced() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");
    write_located(&input, false);

    let refused = run(&input, &output, reencode(Reencode::default()), false);
    let err = refused.result.err().expect("unindexed input is refused");
    assert!(
        matches!(err.kind(), pbfhogg::ErrorKind::MissingIndexdata { .. }),
        "{err:?}"
    );
    assert!(!output.exists(), "a refused run publishes nothing");

    let forced = run(&input, &output, reencode(Reencode::default()), true);
    assert_eq!(forced.result.expect("forced").elements_written, 13);
}

/// The block cap is a re-encode setting: a re-encode reports the one it
/// used, the header-only passthrough reports none.
#[test]
fn only_a_reencode_reports_a_block_cap() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");
    write_located(&input, true);
    let cap = |counters: &[(String, i64)]| -> Vec<i64> {
        counters
            .iter()
            .filter(|(name, _)| name == "degrade_block_cap")
            .map(|(_, value)| *value)
            .collect()
    };

    let reencoded = run(
        &input,
        &output,
        reencode(Reencode {
            block_cap: std::num::NonZeroUsize::new(4).expect("non-zero"),
            ..Reencode::default()
        }),
        false,
    );
    reencoded.result.expect("reencode");
    assert_eq!(cap(&reencoded.counters), [4]);

    let passthrough = run(
        &input,
        &output,
        DegradeFlags {
            strip_indexdata: true,
            ..DegradeFlags::default()
        },
        false,
    );
    passthrough.result.expect("passthrough");
    assert_eq!(cap(&passthrough.counters), Vec::<i64>::new());
}
