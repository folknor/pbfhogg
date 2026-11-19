//! Ring assembly from member ways, by node topology.
//!
//! Members are joined by node ID, never by coordinate: two distinct nodes at
//! one coordinate are two vertices. Every member contributes the edges
//! between its consecutive distinct node IDs. A closed member (first ID equal
//! to last) is its own graph, so an explicitly authored ring never pairs with
//! another member; all open members share one graph.
//!
//! Each graph is cut into its biconnected components (blocks):
//!
//! - a block of one edge is a bridge, on no ring, and is dropped, as an
//!   unclosed chain always was;
//! - a block in which every vertex has degree two is a simple cycle, and is
//!   exactly one ring whatever order the members came in;
//! - any other block (a theta, several paths between two nodes, a branching
//!   even block) has no unique ring decomposition, and the input is refused.
//!
//! A ring that touches itself only at a node is therefore split at that node
//! into its lobes. The rest is refused rather than guessed: a repeated
//! segment (a retrace or spike), a ring that revisits a coordinate through
//! distinct nodes, a ring of fewer than three distinct coordinates, and two
//! rings of one connected graph whose interiors are not provably disjoint
//! (splitting a nested lobe off its walk would turn the walk's even-odd hole
//! into an area of its own).
//!
//! Geometric validity beyond that is not checked: rings that cross, or touch
//! other than at a shared node, are returned as walked.
//!
//! The work is linear in the member refs, apart from the pairwise
//! disjointness check between rings of one connected graph.

use rustc_hash::{FxHashMap, FxHashSet};

/// One member way: its node IDs and their coordinates, one per ref, in
/// source order. Coordinates are `(lat_e7, lon_e7)`.
#[derive(Debug, Clone, Copy)]
pub struct RingMember<'a> {
    /// Node IDs, one per ref.
    pub nodes: &'a [i64],
    /// `(lat_e7, lon_e7)` of each ref, the same length as `nodes`.
    pub coords: &'a [(i32, i32)],
}

/// Why [`assemble_rings`] refused its members. Every refusal is of the whole
/// input: no partial ring set is returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RingRefusal {
    /// `nodes` and `coords` of a member differ in length.
    MismatchedMember,
    /// A block that is not a simple cycle: its rings could be paired more
    /// than one way. A segment joined twice (a retrace, a spike, two
    /// members along one segment) inside a larger block lands here; a block
    /// of only the two copies is a [`RingRefusal::DegenerateRing`].
    AmbiguousJunction,
    /// A ring revisits a coordinate through a distinct node.
    SelfTouchingRing,
    /// A ring has fewer than three distinct coordinates.
    DegenerateRing,
    /// Two rings of one connected graph are not provably disjoint.
    NestedRings,
}

/// Assemble the closed rings of `members`.
///
/// Each ring is explicitly closed (its last vertex repeats its first), starts
/// at its earliest edge in source order (member index, then position within
/// the member) and runs in that edge's source direction; rings are returned
/// in the same order. The module documentation states which inputs are
/// refused.
pub fn assemble_rings(members: &[RingMember<'_>]) -> Result<Vec<Ring>, RingRefusal> {
    let mut rings: Vec<KeyedRing> = Vec::new();
    let mut open = Vec::new();
    for (index, member) in members.iter().enumerate() {
        if member.nodes.len() != member.coords.len() {
            return Err(RingRefusal::MismatchedMember);
        }
        let closed = member.nodes.len() > 1 && member.nodes.first() == member.nodes.last();
        if closed {
            // The usual case: no coordinate repeats, so no node does, and
            // the member is one simple ring as written.
            if member.coords.windows(2).all(|w| w[0] != w[1])
                && check_coordinate_runs(member.coords).is_ok()
            {
                rings.push(((index, 0), member.coords.to_vec()));
                continue;
            }
            Graph::build(&[(index, *member)]).rings(&mut rings)?;
        } else {
            open.push((index, *member));
        }
    }
    Graph::build(&open).rings(&mut rings)?;
    rings.sort_by_key(|(key, _)| *key);
    Ok(rings.into_iter().map(|(_, ring)| ring).collect())
}

/// An explicitly closed ring of `(lat_e7, lon_e7)` vertices.
pub type Ring = Vec<(i32, i32)>;

/// A ring and its source-order key (member index, position).
type KeyedRing = ((usize, usize), Ring);

struct Edge {
    from: usize,
    to: usize,
    /// (member index, position of `from` in the member): source order.
    key: (usize, usize),
}

struct Graph {
    node_ids: Vec<i64>,
    coords: Vec<(i32, i32)>,
    edges: Vec<Edge>,
    /// Per vertex: (neighbour, edge index).
    adjacency: Vec<Vec<(usize, usize)>>,
}

impl Graph {
    /// Repeated segments are kept as parallel edges: the block rule refuses
    /// them (see [`RingRefusal::AmbiguousJunction`]).
    fn build(members: &[(usize, RingMember<'_>)]) -> Self {
        let refs: usize = members.iter().map(|(_, m)| m.nodes.len()).sum();
        let mut index_of: FxHashMap<i64, usize> =
            FxHashMap::with_capacity_and_hasher(refs, Default::default());
        let mut graph = Graph {
            node_ids: Vec::with_capacity(refs),
            coords: Vec::with_capacity(refs),
            edges: Vec::with_capacity(refs),
            adjacency: Vec::with_capacity(refs),
        };
        for &(member_index, member) in members {
            let mut previous: Option<usize> = None;
            for (position, (&id, &coord)) in member.nodes.iter().zip(member.coords).enumerate() {
                let vertex = *index_of.entry(id).or_insert_with(|| {
                    graph.node_ids.push(id);
                    graph.coords.push(coord);
                    graph.adjacency.push(Vec::new());
                    graph.node_ids.len() - 1
                });
                if let Some(from) = previous
                    && from != vertex
                {
                    let edge = graph.edges.len();
                    graph.edges.push(Edge {
                        from,
                        to: vertex,
                        key: (member_index, position - 1),
                    });
                    graph.adjacency[from].push((vertex, edge));
                    graph.adjacency[vertex].push((from, edge));
                }
                previous = Some(vertex);
            }
        }
        graph
    }

    /// Append this graph's rings, each with its source-order key.
    fn rings(&self, out: &mut Vec<KeyedRing>) -> Result<(), RingRefusal> {
        let mut scratch = WalkScratch {
            degree: vec![0; self.node_ids.len()],
            block_of: vec![usize::MAX; self.edges.len()],
        };
        let mut block_id = 0;
        // Rings per connected component, for the disjointness check.
        for component in self.blocks() {
            let mut walked: Vec<WalkedRing> = Vec::new();
            for block in component {
                if block.len() < 2 {
                    continue;
                }
                walked.push(self.walk(&block, block_id, &mut scratch)?);
                block_id += 1;
            }
            if walked.len() > 1 {
                for ring in &mut walked {
                    ring.node_set = ring.nodes.iter().copied().collect();
                }
            }
            for (i, a) in walked.iter().enumerate() {
                for b in &walked[i + 1..] {
                    if !outside(a, b) || !outside(b, a) {
                        return Err(RingRefusal::NestedRings);
                    }
                }
            }
            out.extend(walked.into_iter().map(|ring| (ring.key, ring.coords)));
        }
        Ok(())
    }

    /// Biconnected components as edge lists, grouped by connected component.
    /// Iterative Tarjan. The parent is skipped by edge index, not by vertex,
    /// so a parallel edge back to the parent is a back edge and puts both
    /// copies in one block.
    fn blocks(&self) -> Vec<Vec<Vec<usize>>> {
        let n = self.node_ids.len();
        let mut disc = vec![0_usize; n];
        let mut low = vec![0_usize; n];
        let mut time = 0_usize;
        let mut components = Vec::new();
        let mut edge_stack: Vec<usize> = Vec::new();
        // (vertex, edge it was entered by, next adjacency position)
        let mut frames: Vec<(usize, Option<usize>, usize)> = Vec::new();
        for root in 0..n {
            if disc[root] != 0 {
                continue;
            }
            time += 1;
            disc[root] = time;
            low[root] = time;
            frames.push((root, None, 0));
            let mut blocks = Vec::new();
            while let Some(frame) = frames.last_mut() {
                let (v, parent_edge, next) = *frame;
                if let Some(&(w, edge)) = self.adjacency[v].get(next) {
                    frame.2 += 1;
                    if Some(edge) == parent_edge {
                        continue;
                    }
                    if disc[w] == 0 {
                        edge_stack.push(edge);
                        time += 1;
                        disc[w] = time;
                        low[w] = time;
                        frames.push((w, Some(edge), 0));
                    } else if disc[w] < disc[v] {
                        edge_stack.push(edge);
                        low[v] = low[v].min(disc[w]);
                    }
                    continue;
                }
                frames.pop();
                let Some(&(u, _, _)) = frames.last() else {
                    continue;
                };
                low[u] = low[u].min(low[v]);
                if low[v] >= disc[u]
                    && let Some(entry) = parent_edge
                {
                    let mut block = Vec::new();
                    while let Some(edge) = edge_stack.pop() {
                        block.push(edge);
                        if edge == entry {
                            break;
                        }
                    }
                    blocks.push(block);
                }
            }
            if !blocks.is_empty() {
                components.push(blocks);
            }
        }
        components
    }

    /// Walk one block of two or more edges as a ring, from its earliest edge
    /// in source order and in that edge's direction.
    fn walk(
        &self,
        block: &[usize],
        block_id: usize,
        scratch: &mut WalkScratch,
    ) -> Result<WalkedRing, RingRefusal> {
        for &edge in block {
            scratch.block_of[edge] = block_id;
            scratch.degree[self.edges[edge].from] += 1;
            scratch.degree[self.edges[edge].to] += 1;
        }
        let branching = block.iter().any(|&edge| {
            scratch.degree[self.edges[edge].from] != 2 || scratch.degree[self.edges[edge].to] != 2
        });
        for &edge in block {
            scratch.degree[self.edges[edge].from] = 0;
            scratch.degree[self.edges[edge].to] = 0;
        }
        if branching {
            return Err(RingRefusal::AmbiguousJunction);
        }
        let mut current = block
            .iter()
            .copied()
            .min_by_key(|&edge| self.edges[edge].key)
            .ok_or(RingRefusal::DegenerateRing)?;
        let key = self.edges[current].key;
        let start = self.edges[current].from;
        let mut vertex = start;
        let mut nodes = Vec::with_capacity(block.len());
        let mut coords = Vec::with_capacity(block.len() + 1);
        for _ in 0..block.len() {
            nodes.push(self.node_ids[vertex]);
            coords.push(self.coords[vertex]);
            let edge = &self.edges[current];
            vertex = if edge.from == vertex {
                edge.to
            } else {
                edge.from
            };
            let Some(&(_, next)) = self.adjacency[vertex]
                .iter()
                .find(|&&(_, e)| e != current && scratch.block_of[e] == block_id)
            else {
                return Err(RingRefusal::AmbiguousJunction);
            };
            current = next;
        }
        if vertex != start {
            return Err(RingRefusal::AmbiguousJunction);
        }
        coords.push(self.coords[start]);
        check_coordinate_runs(&coords)?;
        Ok(WalkedRing {
            key,
            node_set: FxHashSet::default(),
            nodes,
            coords,
        })
    }
}

/// Per-graph buffers for [`Graph::walk`]: block degree per vertex (reset
/// after each block) and the block each edge was assigned to.
struct WalkScratch {
    degree: Vec<u32>,
    block_of: Vec<usize>,
}

struct WalkedRing {
    key: (usize, usize),
    /// Node ID of each open vertex, in walk order.
    nodes: Vec<i64>,
    /// `nodes` as a set, filled only when the ring shares a connected
    /// graph with another ring.
    node_set: FxHashSet<i64>,
    /// Explicitly closed: `coords[i]` is the coordinate of `nodes[i]`.
    coords: Vec<(i32, i32)>,
}

/// A closed ring must visit each coordinate in one cyclic run: distinct
/// nodes at one coordinate are allowed only next to each other. It must
/// also have at least three distinct coordinates.
fn check_coordinate_runs(closed: &[(i32, i32)]) -> Result<(), RingRefusal> {
    let open = &closed[..closed.len().saturating_sub(1)];
    let n = open.len();
    // Rotate to begin a run, so no run wraps the end of the slice.
    let Some(first_break) = (0..n).find(|&i| open[i] != open[(i + n - 1) % n]) else {
        return Err(RingRefusal::DegenerateRing);
    };
    let mut seen: FxHashSet<(i32, i32)> =
        FxHashSet::with_capacity_and_hasher(n, Default::default());
    let mut previous: Option<(i32, i32)> = None;
    for offset in 0..n {
        let coord = open[(first_break + offset) % n];
        if previous == Some(coord) {
            continue;
        }
        if !seen.insert(coord) {
            return Err(RingRefusal::SelfTouchingRing);
        }
        previous = Some(coord);
    }
    if seen.len() < 3 {
        return Err(RingRefusal::DegenerateRing);
    }
    Ok(())
}

const HALF_TURN_E7: i64 = 1_800_000_000;
const FULL_TURN_E7: i64 = 3_600_000_000;

/// The ring in the planar frame the antimeridian-aware predicate uses, as
/// `(lon, lat)` in E7: as given, or with longitudes unwrapped when any edge
/// (closing edge included) spans more than half a turn. The flag says the
/// predicate retries a point a full turn either way.
fn frame(ring: &[(i32, i32)]) -> (Vec<(i64, i64)>, bool) {
    let raw: Vec<(i64, i64)> = ring
        .iter()
        .map(|&(lat, lon)| (i64::from(lon), i64::from(lat)))
        .collect();
    let crosses = raw
        .windows(2)
        .any(|w| (w[1].0 - w[0].0).abs() > HALF_TURN_E7)
        || match (raw.first(), raw.last()) {
            (Some(a), Some(b)) => (b.0 - a.0).abs() > HALF_TURN_E7,
            _ => false,
        };
    if !crosses {
        return (raw, false);
    }
    let mut out = Vec::with_capacity(raw.len());
    let mut previous: Option<i64> = None;
    for (lon, lat) in raw {
        let mut adjusted = lon;
        if let Some(p) = previous {
            while adjusted - p > HALF_TURN_E7 {
                adjusted -= FULL_TURN_E7;
            }
            while adjusted - p < -HALF_TURN_E7 {
                adjusted += FULL_TURN_E7;
            }
        }
        out.push((adjusted, lat));
        previous = Some(adjusted);
    }
    (out, true)
}

#[derive(PartialEq, Eq)]
enum Side {
    Inside,
    Outside,
    Boundary,
}

/// Exact even-odd classification of a point, in doubled E7 coordinates,
/// against a ring in doubled E7 coordinates.
fn classify(px: i64, py: i64, ring: &[(i64, i64)]) -> Side {
    let n = ring.len();
    let mut inside = false;
    let mut j = n - 1;
    for i in 0..n {
        let (ax, ay) = ring[j];
        let (bx, by) = ring[i];
        j = i;
        let cross =
            i128::from(bx - ax) * i128::from(py - ay) - i128::from(by - ay) * i128::from(px - ax);
        if cross == 0
            && px >= ax.min(bx)
            && px <= ax.max(bx)
            && py >= ay.min(by)
            && py <= ay.max(by)
        {
            return Side::Boundary;
        }
        if (ay > py) != (by > py) {
            // px < x-intercept, sign-corrected for the edge direction.
            let left = i128::from(px - ax) * i128::from(by - ay);
            let right = i128::from(bx - ax) * i128::from(py - ay);
            if (by > ay && left < right) || (by < ay && left > right) {
                inside = !inside;
            }
        }
    }
    if inside { Side::Inside } else { Side::Outside }
}

/// True when `a` is provably outside `b`: an edge midpoint of `a` whose
/// endpoints are not nodes of `b` is strictly outside `b` in the predicate's
/// frame. Two rings that share only cut vertices have each boundary, minus
/// those vertices, on one side of the other, so one witness decides. No
/// witness, or a witness on the boundary, is not a proof.
fn outside(a: &WalkedRing, b: &WalkedRing) -> bool {
    let (a_frame, _wraps) = frame(&a.coords);
    let (b_frame, b_wraps) = frame(&b.coords);
    let doubled: Vec<(i64, i64)> = b_frame.iter().map(|&(x, y)| (2 * x, 2 * y)).collect();
    let ids = &a.nodes;
    for i in 0..a_frame.len().saturating_sub(1) {
        if b.node_set.contains(&ids[i]) || b.node_set.contains(&ids[(i + 1) % ids.len()]) {
            continue;
        }
        // Back to the [-180, 180] longitude a query would carry.
        let mut px = a_frame[i].0 + a_frame[i + 1].0;
        while px > 2 * HALF_TURN_E7 {
            px -= 2 * FULL_TURN_E7;
        }
        while px < -2 * HALF_TURN_E7 {
            px += 2 * FULL_TURN_E7;
        }
        let py = a_frame[i].1 + a_frame[i + 1].1;
        let shifts: &[i64] = if b_wraps {
            &[0, 2 * FULL_TURN_E7, -2 * FULL_TURN_E7]
        } else {
            &[0]
        };
        let mut all_outside = true;
        for &shift in shifts {
            if classify(px + shift, py, &doubled) != Side::Outside {
                all_outside = false;
            }
        }
        return all_outside;
    }
    false
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{Ring, RingMember, RingRefusal, assemble_rings};

    type Member = (Vec<i64>, Vec<(i32, i32)>);

    /// Members as (node IDs, coordinates) pairs.
    fn run(members: &[Member]) -> Result<Vec<Ring>, RingRefusal> {
        let members: Vec<RingMember<'_>> = members
            .iter()
            .map(|(nodes, coords)| RingMember { nodes, coords })
            .collect();
        assemble_rings(&members)
    }

    /// A member through the named nodes of `at`.
    fn way(ids: &[i64], at: impl Fn(i64) -> (i32, i32)) -> Member {
        (ids.to_vec(), ids.iter().map(|&id| at(id)).collect())
    }

    /// A square `[0, 10] x [0, 10]` with named corners and midpoints, as
    /// `(lat, lon)`: 1 (0,0), 2 (0,10), 3 (10,10), 4 (10,0), 5 (5,10),
    /// 6 (5,0); other IDs as the tests place them.
    fn grid(id: i64) -> (i32, i32) {
        match id {
            1 => (0, 0),
            2 => (0, 10),
            3 => (10, 10),
            4 => (10, 0),
            5 => (5, 10),
            6 => (5, 0),
            _ => (
                i32::try_from(id / 100).unwrap(),
                i32::try_from(id % 100).unwrap(),
            ),
        }
    }

    #[test]
    fn split_members_close_one_ring_from_the_earliest_edge() {
        let rings = run(&[
            way(&[1, 2], grid),
            way(&[3, 2], grid),
            way(&[3, 4, 1], grid),
        ])
        .unwrap();
        assert_eq!(
            rings,
            vec![vec![(0, 0), (0, 10), (10, 10), (10, 0), (0, 0)]]
        );
    }

    #[test]
    fn an_open_chain_is_no_ring_and_nothing_is_nothing() {
        assert_eq!(
            run(&[way(&[1, 2], grid), way(&[2, 3], grid)]),
            Ok(Vec::new())
        );
        assert_eq!(run(&[]), Ok(Vec::new()));
    }

    /// Four ways between the same two nodes pair more than one way; the
    /// answer must not depend on which pairing the member order suggests.
    #[test]
    fn several_paths_between_two_nodes_are_refused_in_any_order() {
        let mut ways = vec![
            way(&[1, 102, 2], grid),
            way(&[1, 304, 2], grid),
            way(&[1, 506, 2], grid),
            way(&[1, 708, 2], grid),
        ];
        for _ in 0..4 {
            assert_eq!(run(&ways), Err(RingRefusal::AmbiguousJunction));
            ways.rotate_left(1);
        }
        // A theta is refused too.
        assert_eq!(
            run(&[
                way(&[1, 2], grid),
                way(&[2, 3, 4, 1], grid),
                way(&[2, 4], grid)
            ]),
            Err(RingRefusal::AmbiguousJunction)
        );
    }

    /// A leaf and many loops at its end node, beside a disconnected closed
    /// ring: every loop and the ring are kept, the leaf is dropped.
    #[test]
    fn a_leaf_beside_loops_keeps_every_ring() {
        let mut ways = vec![way(&[9_999, 1], grid)];
        for k in 1..=20 {
            ways.push(way(&[1, 100 * k + 1, 100 * k + 2, 1], grid));
        }
        ways.push(way(&[5_050, 5_060, 6_060, 5_050], grid));
        let rings = run(&ways).unwrap();
        assert_eq!(rings.len(), 21);
        assert_eq!(rings[20], vec![(50, 50), (50, 60), (60, 60), (50, 50)]);
    }

    /// A self-closed member met by a chain at one node is its own ring, not
    /// a lobe of a figure-eight through the chain.
    #[test]
    fn a_loop_met_mid_chain_is_its_own_ring() {
        // Chain round the square through 5; a loop at 5 outside the square.
        let rings = run(&[
            way(&[1, 2, 5], grid),
            way(&[1_520, 1_525, 1_020, 5], grid),
            way(&[5, 1_520], grid),
            way(&[5, 3, 4, 1], grid),
        ])
        .unwrap();
        // The loop's open ways meet the chain only at 5, a cut vertex.
        assert_eq!(rings.len(), 2);
        let rings = run(&[
            way(&[1, 2, 5, 3, 4, 1], grid),
            way(&[5, 1_520, 1_525, 5], grid),
        ])
        .unwrap();
        assert_eq!(
            rings,
            vec![
                vec![(0, 0), (0, 10), (5, 10), (10, 10), (10, 0), (0, 0)],
                vec![(5, 10), (15, 20), (15, 25), (5, 10)],
            ]
        );
    }

    /// Two triangles touching at one node, drawn by open ways, are two
    /// rings, not one figure-eight.
    #[test]
    fn touching_rings_split_at_their_shared_node() {
        let at = |id: i64| match id {
            1 => (0, 0),
            2 => (10, 0),
            3 => (5, 5),
            4 => (10, 10),
            5 => (0, 10),
            _ => unreachable!(),
        };
        let rings = run(&[
            way(&[1, 2], at),
            way(&[2, 3], at),
            way(&[3, 4], at),
            way(&[4, 5], at),
            way(&[5, 3], at),
            way(&[3, 1], at),
        ])
        .unwrap();
        assert_eq!(
            rings,
            vec![
                vec![(0, 0), (10, 0), (5, 5), (0, 0)],
                vec![(5, 5), (10, 10), (0, 10), (5, 5)],
            ]
        );
    }

    #[test]
    fn a_retrace_is_refused() {
        assert_eq!(
            run(&[way(&[1, 2, 1], grid)]),
            Err(RingRefusal::DegenerateRing)
        );
        assert_eq!(
            run(&[way(&[1, 2], grid), way(&[2, 1], grid)]),
            Err(RingRefusal::DegenerateRing)
        );
        assert_eq!(
            run(&[way(&[1, 2, 3, 1, 2, 3, 1], grid)]),
            Err(RingRefusal::AmbiguousJunction)
        );
        assert_eq!(
            run(&[way(&[1, 2, 3], grid), way(&[3, 2, 1], grid)]),
            Err(RingRefusal::DegenerateRing)
        );
        // A spike on an otherwise simple ring: its two copies are a block
        // of their own at the cut vertex.
        assert_eq!(
            run(&[way(&[1, 2, 3, 708, 3, 4, 1], grid)]),
            Err(RingRefusal::DegenerateRing)
        );
    }

    /// Two distinct nodes at one coordinate do not close a ring.
    #[test]
    fn an_equal_coordinate_on_a_distinct_node_does_not_close() {
        let at = |id: i64| if id == 7 { grid(1) } else { grid(id) };
        assert_eq!(
            run(&[way(&[1, 2, 3], grid), way(&[3, 4, 7], at)]),
            Ok(Vec::new())
        );
    }

    /// One ring through two nodes at one point is a figure-eight whose
    /// lobes cancel in the area: refused.
    #[test]
    fn a_ring_revisiting_a_coordinate_is_refused() {
        let at = |id: i64| match id {
            1 | 4 => (0, 0),
            2 => (10, 10),
            3 => (10, -10),
            5 => (-10, -10),
            6 => (-10, 10),
            _ => unreachable!(),
        };
        assert_eq!(
            run(&[way(&[1, 2, 3, 4, 5, 6, 1], at)]),
            Err(RingRefusal::SelfTouchingRing)
        );
    }

    /// A closed way whose walk returns to one node splits into lobes when
    /// they are disjoint, and is refused when one lobe lies in the other
    /// (the walk's even-odd hole would become an area of its own).
    #[test]
    fn lobes_of_one_walk_must_be_disjoint() {
        let at = |id: i64| match id {
            1 => (5, 10),
            2 => (0, 10),
            3 => (0, 0),
            4 => (10, 0),
            5 => (10, 10),
            // Outside the square, right of 1.
            6 => (4, 14),
            7 => (6, 14),
            // Inside the square, left of 1.
            8 => (4, 8),
            9 => (6, 8),
            _ => unreachable!(),
        };
        let rings = run(&[way(&[1, 2, 3, 4, 5, 1, 6, 7, 1], at)]).unwrap();
        assert_eq!(rings.len(), 2);
        assert_eq!(
            run(&[way(&[1, 2, 3, 4, 5, 1, 8, 9, 1], at)]),
            Err(RingRefusal::NestedRings)
        );
    }

    /// A witness on the other ring's boundary is no proof of disjointness:
    /// the inner triangle's only usable edge touches the tip of a notch.
    #[test]
    fn a_witness_on_the_boundary_refuses() {
        // (lat, lon) of the (lon, lat) shape A (0,5), notch tip (7,5).
        let at = |id: i64| match id {
            1 => (5, 0),
            2 => (0, 0),
            3 => (0, 10),
            4 => (4, 10),
            5 => (5, 7),
            6 => (6, 10),
            7 => (10, 10),
            8 => (10, 0),
            9 => (4, 7),
            10 => (6, 7),
            _ => unreachable!(),
        };
        assert_eq!(
            run(&[way(&[1, 2, 3, 4, 5, 6, 7, 8, 1, 9, 10, 1], at)]),
            Err(RingRefusal::NestedRings)
        );
    }

    #[test]
    fn mismatched_members_are_refused() {
        let nodes = [1, 2];
        let coords = [(0, 0)];
        assert_eq!(
            assemble_rings(&[RingMember {
                nodes: &nodes,
                coords: &coords
            }]),
            Err(RingRefusal::MismatchedMember)
        );
    }
}
