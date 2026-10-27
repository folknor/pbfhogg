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

/// Tag key=value counts from a tiny hand-crafted fixture.
#[test]
fn basic_tag_counting() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");

    // 5 nodes with two distinct tags, 3 ways with one common tag.
    // Distribute so counts are non-trivial.
    let mut nodes = generate_nodes(5, 1);
    for (i, n) in nodes.iter_mut().enumerate() {
        n.tags = if i < 3 {
            vec![("amenity", "cafe")]
        } else {
            vec![("shop", "bakery")]
        };
    }
    let mut ways = generate_ways(3, 1_000, 2, 1);
    for w in &mut ways {
        w.tags = vec![("highway", "primary")];
    }
    write_multi_block_test_pbf(&input, &nodes, &ways, &[], 100);

    let counts =
        tags_count(&input, &default_opts(), &crate::common::test_host()).expect("tags_count");

    // With NameAsc sort, the order is amenity=cafe, highway=primary, shop=bakery.
    let find = |key: &str, value: &str| -> Option<&TagCount> {
        counts.iter().find(|t| t.key == key && t.value == value)
    };
    assert_eq!(find("amenity", "cafe").expect("amenity=cafe").count, 3);
    assert_eq!(find("shop", "bakery").expect("shop=bakery").count, 2);
    assert_eq!(
        find("highway", "primary").expect("highway=primary").count,
        3
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

    let mut opts = default_opts();
    opts.parallelism = pbfhogg::Parallelism::from_jobs(1);
    let seq = tags_count(&input, &opts, &crate::common::test_host()).expect("tags_count seq");

    opts.parallelism = pbfhogg::Parallelism::from_jobs(4);
    let par = tags_count(&input, &opts, &crate::common::test_host()).expect("tags_count par");

    // Sort by (key, value) for order-independent comparison - NameAsc
    // sort above guarantees the orderings match, but be explicit.
    let key = |t: &TagCount| (t.key.clone(), t.value.clone());
    let mut seq_sorted = seq;
    let mut par_sorted = par;
    seq_sorted.sort_by_key(key);
    par_sorted.sort_by_key(|t: &TagCount| (t.key.clone(), t.value.clone()));

    assert_eq!(
        seq_sorted.len(),
        par_sorted.len(),
        "count entry cardinality diverges under -j 4"
    );
    for (s, p) in seq_sorted.iter().zip(par_sorted.iter()) {
        assert_eq!(s.key, p.key, "key drift");
        assert_eq!(s.value, p.value, "value drift");
        assert_eq!(
            s.count, p.count,
            "count drift for {}={}: seq={} par={}",
            s.key, s.value, s.count, p.count
        );
    }

    // Sanity: we actually have some counts to compare.
    assert!(
        !seq_sorted.is_empty(),
        "fixture must produce at least one tag count"
    );
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
    let mut counts = tags_count(&input, &opts, &crate::common::test_host()).expect("tags_count");
    counts.sort_by_key(|t: &TagCount| (t.key.clone(), t.value.clone()));
    let got: Vec<_> = counts
        .into_iter()
        .map(|t| (t.key, t.value, t.count))
        .collect();

    assert_eq!(
        got,
        vec![
            ("amenity".to_string(), "cafe".to_string(), 20),
            ("building".to_string(), "yes".to_string(), 20),
        ],
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
    let counts = tags_count(&honest, &default_opts(), &host).expect("honest claims run");
    assert_eq!(counts.len(), 2, "amenity=cafe and highway=primary");

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
