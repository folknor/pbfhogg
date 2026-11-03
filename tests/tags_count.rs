//! Integration tests for `tags_count` (the library backing for
//! `pbfhogg inspect --tags`). `tags_count` had no prior integration
//! coverage, so this file seeds a minimal test plus the parallel
//! classify parity check called out in the testing-infra sprint. The
//! tag-index skip test, which needs the binary, is the CLI package's
//! cli/tests/cli_tags_count.rs.

use crate::common::{generate_nodes, generate_ways, write_multi_block_test_pbf};
use pbfhogg::tags_count::{TagCount, TagCountOptions, TagCountSort, tags_count};

fn default_opts() -> TagCountOptions<'static> {
    TagCountOptions {
        min_count: 0,
        max_count: None,
        sort: TagCountSort::NameAsc,
        expressions: &[],
        type_filter: None,
        direct_io: false,
        force: true,
        parallelism: pbfhogg::Parallelism::from_jobs(1),
    }
}

/// `(key, value, count)` of every row, `None` for a key row, in the order
/// the run returned them.
fn listed(rows: Vec<TagCount>) -> Vec<(String, Option<String>, u64)> {
    rows.into_iter()
        .map(|t| (t.key, t.value, t.count))
        .collect()
}

fn key(k: &str, n: u64) -> (String, Option<String>, u64) {
    (k.to_string(), None, n)
}

fn pair(k: &str, v: &str, n: u64) -> (String, Option<String>, u64) {
    (k.to_string(), Some(v.to_string()), n)
}

/// amenity=cafe 3 and shop=bakery 2 on nodes; highway=primary 3 and
/// highway=secondary 1 on ways, one of the primary ways also `amenity`
/// with the empty value.
fn write_basic(input: &std::path::Path) {
    let mut nodes = generate_nodes(5, 1);
    for (i, n) in nodes.iter_mut().enumerate() {
        n.tags = if i < 3 {
            vec![("amenity", "cafe")]
        } else {
            vec![("shop", "bakery")]
        };
    }
    let mut ways = generate_ways(4, 1_000, 2, 1);
    for (i, w) in ways.iter_mut().enumerate() {
        w.tags = match i {
            0 => vec![("highway", "primary"), ("amenity", "")],
            3 => vec![("highway", "secondary")],
            _ => vec![("highway", "primary")],
        };
    }
    write_multi_block_test_pbf(input, &nodes, &ways, &[], 100);
}

/// osmium's rule: no expression and a key expression count keys; an
/// expression with `=` counts pairs; a tag both select counts in both rows.
/// `NameAsc` puts a key row before its pairs, and the empty value is a pair
/// of its own.
#[test]
fn keys_and_pairs_follow_the_expressions() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    write_basic(&input);
    let host = crate::common::test_host();
    let run = |exprs: &[&str]| {
        let exprs: Vec<String> = exprs.iter().map(|&e| e.to_string()).collect();
        let mut opts = default_opts();
        opts.expressions = &exprs;
        listed(tags_count(&input, &opts, &host).expect("tags_count").rows)
    };

    assert_eq!(
        run(&[]),
        vec![key("amenity", 4), key("highway", 4), key("shop", 2)]
    );
    assert_eq!(run(&["highway"]), vec![key("highway", 4)]);
    assert_eq!(
        run(&["highway=*"]),
        vec![
            pair("highway", "primary", 3),
            pair("highway", "secondary", 1)
        ]
    );
    assert_eq!(
        run(&["highway=primary"]),
        vec![pair("highway", "primary", 3)]
    );
    assert_eq!(
        run(&["amenity", "amenity=*", "w/highway"]),
        vec![
            key("amenity", 4),
            pair("amenity", "", 1),
            pair("amenity", "cafe", 3),
            key("highway", 4),
        ]
    );
    assert_eq!(
        run(&["n/amenity", "w/amenity=*"]),
        vec![key("amenity", 3), pair("amenity", "", 1)],
        "each route's type prefix applies to its own rows"
    );
}

/// The count orders break ties by name, a key row before its pairs; the
/// name orders reverse it whole.
#[test]
fn rows_sort_as_osmium_names_do() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    write_basic(&input);
    let host = crate::common::test_host();
    let exprs: Vec<String> = ["amenity", "amenity=*", "highway"]
        .iter()
        .map(|&e| e.to_string())
        .collect();
    let run = |sort| {
        let mut opts = default_opts();
        opts.expressions = &exprs;
        opts.sort = sort;
        listed(tags_count(&input, &opts, &host).expect("tags_count").rows)
    };
    let (k_amenity, empty, cafe, k_highway) = (
        key("amenity", 4),
        pair("amenity", "", 1),
        pair("amenity", "cafe", 3),
        key("highway", 4),
    );
    assert_eq!(
        run(TagCountSort::CountDesc),
        vec![
            k_amenity.clone(),
            k_highway.clone(),
            cafe.clone(),
            empty.clone()
        ]
    );
    assert_eq!(
        run(TagCountSort::CountAsc),
        vec![
            empty.clone(),
            cafe.clone(),
            k_amenity.clone(),
            k_highway.clone()
        ]
    );
    assert_eq!(
        run(TagCountSort::NameDesc),
        vec![k_highway, cafe, empty, k_amenity]
    );
}

/// The run's figures: each kind of row the run selected, whatever the
/// count thresholds keep, and only the type filter and expressions narrow
/// them; `retained_rows` is the rows returned, of both kinds. None moves
/// when the caller edits the rows afterwards.
#[test]
fn stats_count_distinct_rows_before_the_thresholds() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    write_basic(&input);
    let host = crate::common::test_host();

    let run = |opts: &TagCountOptions<'_>| {
        let counts = tags_count(&input, opts, &host).expect("tags_count");
        assert_eq!(
            counts.stats.retained_rows,
            counts.rows.len() as u64,
            "retained_rows is the rows returned"
        );
        (
            counts.stats.key_rows,
            counts.stats.pair_rows,
            counts.stats.retained_rows,
        )
    };

    // Every threshold sees the same three keys: amenity 4, highway 4, shop 2.
    let mut opts = default_opts();
    assert_eq!(run(&opts), (3, 0, 3));
    opts.min_count = 3;
    assert_eq!(run(&opts), (3, 0, 2));
    opts.min_count = 5;
    assert_eq!(run(&opts), (3, 0, 0), "keys counted, none passed");
    opts.min_count = 0;
    opts.max_count = Some(2);
    assert_eq!(run(&opts), (3, 0, 1));

    // The type filter and the expressions narrow what is counted; the
    // thresholds apply to both kinds of row.
    let mut opts = default_opts();
    opts.type_filter = Some(pbfhogg::TypeFilter {
        nodes: true,
        ways: false,
        relations: false,
    });
    assert_eq!(run(&opts), (2, 0, 2));
    let exprs = vec!["highway".to_string(), "*=*".to_string()];
    let mut opts = default_opts();
    opts.expressions = &exprs;
    assert_eq!(run(&opts), (1, 5, 6));
    opts.min_count = 3;
    assert_eq!(
        run(&opts),
        (1, 5, 3),
        "highway, amenity=cafe, highway=primary"
    );

    // The summary is the run's, not the rows' current state.
    let mut counts = tags_count(&input, &opts, &host).expect("tags_count");
    counts.rows.clear();
    let mut line = Vec::new();
    counts.stats.write_summary(&mut line).expect("write");
    assert_eq!(
        String::from_utf8(line).expect("utf-8"),
        "1 key rows and 5 key/value rows counted, 3 passed the count thresholds\n"
    );
}

/// jobs=1 and jobs=4 must produce identical per-blob classification +
/// per-tag count merges. Multi-blob fixture forces the sharding path
/// to actually split work across workers.
#[test]
fn tags_count_parallel_classify_parity() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");

    // 40 nodes, each alternating between two keys; block_size=10 -> 4
    // node blobs. Each blob gets a representative mix so no worker sees
    // a trivially-empty blob.
    let mut nodes = generate_nodes(40, 1);
    for (i, n) in nodes.iter_mut().enumerate() {
        n.tags = if i % 2 == 0 {
            vec![("amenity", "cafe")]
        } else {
            vec![("amenity", "bar"), ("outdoor_seating", "yes")]
        };
    }
    write_multi_block_test_pbf(&input, &nodes, &[], &[], 10);

    // Keys alone, pairs alone, and both routes at once.
    let selections: [&[&str]; 3] = [&[], &["*=*"], &["amenity", "outdoor_seating=*"]];
    for exprs in selections {
        let exprs: Vec<String> = exprs.iter().map(|&e| e.to_string()).collect();
        let mut opts = default_opts();
        opts.expressions = &exprs;
        opts.parallelism = pbfhogg::Parallelism::from_jobs(1);
        let seq = tags_count(&input, &opts, &crate::common::test_host())
            .expect("tags_count seq")
            .rows;
        opts.parallelism = pbfhogg::Parallelism::from_jobs(4);
        let par = tags_count(&input, &opts, &crate::common::test_host())
            .expect("tags_count par")
            .rows;
        // NameAsc orders both runs the same way, since names are unique.
        let (seq, par) = (listed(seq), listed(par));
        assert_eq!(seq, par, "{exprs:?}: counts diverge under -j 4");
        // Sanity: we actually have some counts to compare.
        assert!(!seq.is_empty(), "{exprs:?}: fixture must produce a count");
    }
}

/// The blob filter must fall back to decoding everything when an expression's
/// key matcher cannot be expressed as key-set membership.
///
/// `*=yes` has an `Always` key matcher, which constrains no key at all. If the
/// filter kept the keys contributed by the *other* expression it would skip
/// blobs the unconstrained one still matches - the filter is a union.
#[test]
fn an_unconstrainable_key_matcher_does_not_skip_blobs() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");

    let mut nodes = generate_nodes(40, 1);
    for (i, n) in nodes.iter_mut().enumerate() {
        n.tags = if i < 20 {
            vec![("amenity", "cafe")]
        } else {
            vec![("building", "yes")]
        };
    }
    write_multi_block_test_pbf(&input, &nodes, &[], &[], 10);

    let exprs = vec!["amenity=cafe".to_string(), "*=yes".to_string()];
    let mut opts = default_opts();
    opts.expressions = &exprs;
    let got = listed(
        tags_count(&input, &opts, &crate::common::test_host())
            .expect("tags_count")
            .rows,
    );

    assert_eq!(
        got,
        vec![pair("amenity", "cafe", 20), pair("building", "yes", 20)],
        "the `building=yes` blobs carry no `amenity` key, so a filter that \
         kept `amenity` from the first expression would skip them and lose \
         every `*=yes` match in them"
    );
}

/// Frame ordinals of the claim fixture: header, nodes, ways.
const NODE_BLOB: usize = 1;
const WAY_BLOB: usize = 2;

/// The fixture with frame `blob`'s indexdata kind byte set to `kind`
/// (0 node, 1 way): a lie no producer writes. `None` leaves it honest.
fn claim_fixture(dir: &std::path::Path, forge: Option<(usize, u8)>) -> std::path::PathBuf {
    let honest = dir.join("honest.osm.pbf");
    let node = |id| crate::common::TestNode {
        id,
        lat: 0,
        lon: 0,
        tags: vec![("amenity", "cafe")],
        meta: None,
    };
    let way = crate::common::TestWay {
        id: 10,
        refs: vec![1, 2],
        tags: vec![("highway", "primary")],
        meta: None,
    };
    crate::common::write_test_pbf_sorted(&honest, &[node(1), node(2)], &[way], &[]);
    let Some((blob, kind)) = forge else {
        return honest;
    };
    let bytes = std::fs::read(&honest).expect("read the fixture");
    let lying = crate::common::adversarial::mutate_blob_header_indexdata(&bytes, blob, |index| {
        index[1] = kind;
    });
    let path = dir.join(format!("blob-{blob}-claims-{kind}.osm.pbf"));
    std::fs::write(&path, lying).expect("write the forged file");
    path
}

fn assert_contradiction(
    err: &pbfhogg::Error,
    claimed: pbfhogg::ElementKind,
    found: pbfhogg::ElementKind,
    blob: usize,
) {
    assert!(
        matches!(err.kind(), pbfhogg::ErrorKind::IndexdataMismatch { .. }),
        "{err:?}"
    );
    assert_eq!(
        err.kind_contradiction(),
        Some(pbfhogg::KindContradiction { claimed, found })
    );
    assert_eq!(err.blob_index(), Some(blob as u64));
}

/// A blob decoded under a kind claim that holds another kind is refused,
/// whether or not a type filter narrowed the schedule; the honest file
/// counts both kinds. This is also `inspect --tags --type`.
#[test]
fn tags_count_refuses_a_blob_holding_another_kind_than_it_claims() {
    let dir = crate::common::test_scratch();
    let host = crate::common::test_host();

    let honest = claim_fixture(dir.path(), None);
    let counts = tags_count(&honest, &default_opts(), &host)
        .expect("honest claims run")
        .rows;
    assert_eq!(counts.len(), 2, "the keys amenity and highway");

    // No filter: every blob is scheduled, the way blob claims nodes.
    let dir2 = crate::common::test_scratch();
    let forged = claim_fixture(dir2.path(), Some((WAY_BLOB, 0)));
    let Err(err) = tags_count(&forged, &default_opts(), &host) else {
        panic!("a lying claim must be refused");
    };
    assert_contradiction(
        &err,
        pbfhogg::ElementKind::Node,
        pbfhogg::ElementKind::Way,
        WAY_BLOB,
    );

    // `--type way`: the node blob claiming ways is scheduled and decoded.
    let dir3 = crate::common::test_scratch();
    let forged = claim_fixture(dir3.path(), Some((NODE_BLOB, 1)));
    let mut opts = default_opts();
    opts.type_filter = Some(pbfhogg::TypeFilter {
        nodes: false,
        ways: true,
        relations: false,
    });
    let Err(err) = tags_count(&forged, &opts, &host) else {
        panic!("a lying claim must be refused");
    };
    assert_contradiction(
        &err,
        pbfhogg::ElementKind::Way,
        pbfhogg::ElementKind::Node,
        NODE_BLOB,
    );
}
