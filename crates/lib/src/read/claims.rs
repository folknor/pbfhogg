//! The metadata an element claims: the one reading of `Info` and
//! `DenseInfo` that every output which prints or compares metadata shares.
//!
//! Plain `Info` fields are individually optional on the wire, so presence is
//! the wire's, except the version, where 0 is how PBF writers spell "no
//! version" ([`Info::claimed_version`]). A `DenseInfo` column decodes a value
//! for every slot, so presence is read from the values
//! ([`DenseNodeInfo::claims`](crate::DenseNodeInfo::claims), which builds a
//! [`MetaClaims`] too). The two readings agree on every element both
//! encodings can express; a dense slot cannot say "absent" for a field of a
//! versioned element whose column the group carries, so it claims every
//! present column there (and `visible` as `true` when that column is absent).

use crate::Info;
use crate::error::Result;

/// A raw timestamp scaled by the block's `date_granularity` to
/// milliseconds, exactly. A scaling that overflows `i64` is an error: the
/// public accessors wrap there, and a wrapped value reads as a plausible
/// date (raw `i64::MAX` at granularity 1000 is -1000 ms), or as exactly 0,
/// which a versionless dense slot would take for padding. Every output that
/// prints, compares or selects by a claimed timestamp goes through this.
#[inline]
pub(crate) fn scale_milli_timestamp(raw: i64, date_granularity: i64) -> Result<i64> {
    raw.checked_mul(date_granularity).ok_or_else(|| {
        crate::error::new_wire_error(
            "timestamp scaled by the block's date_granularity overflows i64 milliseconds; \
             the blob's timestamp or granularity is corrupt",
        )
    })
}

/// What one element claims, field by field. `None` is "not claimed".
#[derive(Clone, Debug)]
pub(crate) struct MetaClaims<'a> {
    pub(crate) version: Option<i32>,
    /// The claimed timestamp in milliseconds; a scaling that overflows
    /// `i64` is an `Err` value ([`scale_milli_timestamp`]), judged by each
    /// output like a bad user index.
    pub(crate) milli_timestamp: Option<Result<i64>>,
    pub(crate) changeset: Option<i64>,
    pub(crate) uid: Option<i32>,
    /// The claimed user name, resolved; a bad string index is an `Err`
    /// value, which each output judges for itself.
    pub(crate) user: Option<Result<&'a str>>,
    /// `visible` as the element states it; absent is not the same claim as
    /// an explicit `true`.
    pub(crate) visible: Option<bool>,
}

impl<'a> MetaClaims<'a> {
    /// The claims of a plain node, way or relation.
    pub(crate) fn from_info(info: &Info<'a>) -> Self {
        Self {
            version: info.claimed_version(),
            milli_timestamp: info.claimed_milli_timestamp(),
            changeset: info.changeset(),
            uid: info.uid(),
            user: info.user(),
            visible: info.visible_opt(),
        }
    }
}
