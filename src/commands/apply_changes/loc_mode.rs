//! How a way write treats LocationsOnWays.
//!
//! Every way emission site - surviving base ways in a rewritten blob, OSC
//! way creates and modifications from workers, gap and trailing creates
//! from the drain - takes a [`LocMode`] rather than an optional coordinate
//! map. An `Option` made "LocationsOnWays is on but the map is missing"
//! indistinguishable from "LocationsOnWays is off", so a missing map
//! silently wrote ways without their inline coordinates under a header
//! that still advertised `LocationsOnWays`. With the enum that state has
//! its own name, `LocMode::Pending`, and writing a way in it is an error.

use rustc_hash::FxHashMap;

use super::Result;

/// Merged node-id to `(lat, lon)` decimicrodegree map published at the
/// node-to-way barrier.
pub(super) type LocMap = FxHashMap<i64, (i32, i32)>;

#[derive(Clone, Copy)]
pub(super) enum LocMode<'a> {
    /// The output carries no LocationsOnWays: ways are written refs-only.
    Off,
    /// `--locations-on-ways` is on, but the node-to-way barrier has not
    /// published the merged coordinate map yet. That is the normal state
    /// while node blobs are processed; a way reaching a writer in this
    /// state means a routing invariant broke (for example a blob whose
    /// indexdata labels it as nodes but which holds ways).
    Pending,
    /// `--locations-on-ways` is on and the merged map is published.
    Required(&'a LocMap),
}

impl<'a> LocMode<'a> {
    /// Derive the mode from the run flag and whatever map is published.
    pub(super) fn new(locations_on_ways: bool, map: Option<&'a LocMap>) -> Self {
        match (locations_on_ways, map) {
            (false, _) => Self::Off,
            (true, Some(map)) => Self::Required(map),
            (true, None) => Self::Pending,
        }
    }

    /// Gate one way write. `Ok(None)` means write it refs-only,
    /// `Ok(Some(map))` means write it with locations, and `Pending` is a
    /// hard error naming the way.
    pub(super) fn for_way(self, way_id: i64) -> Result<Option<&'a LocMap>> {
        match self {
            Self::Off => Ok(None),
            Self::Required(map) => Ok(Some(map)),
            Self::Pending => Err(crate::Error::internal(format!(
                "apply-changes with locations_on_ways: way {way_id} reached the writer before \
                 the node coordinate map was published; refusing to write it without its \
                 LocationsOnWays coordinates"
            ))),
        }
    }
}
