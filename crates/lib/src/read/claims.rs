//! The metadata an element claims: the one reading of `Info` and
//! `DenseInfo` that every output which prints or compares metadata shares.
//!
//! Plain `Info` fields are individually optional on the wire, so presence is
//! the wire's, except the version, where 0 is how PBF writers spell "no
//! version" ([`Info::claimed_version`]). A `DenseInfo` column decodes a value
//! for every slot, so presence is read from the values
//! ([`DenseNodeInfo::claims`]). The two readings agree on every element both
//! encodings can express; a dense slot cannot say "absent" for a field of a
//! versioned element, so it claims every field there.

use crate::error::Result;
use crate::{DenseNodeInfo, Info};

/// What one element claims, field by field. `None` is "not claimed".
#[derive(Clone, Debug)]
pub(crate) struct MetaClaims<'a> {
    pub(crate) version: Option<i32>,
    pub(crate) milli_timestamp: Option<i64>,
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
            milli_timestamp: info.milli_timestamp(),
            changeset: info.changeset(),
            uid: info.uid(),
            user: info.user(),
            visible: info.visible_opt(),
        }
    }

    /// The claims of a dense node slot; `None` when it claims nothing (a
    /// negative version). See [`DenseNodeInfo::claims`].
    pub(crate) fn from_dense(info: &DenseNodeInfo<'a>) -> Option<Self> {
        let claims = info.claims()?;
        Some(Self {
            version: claims.version,
            milli_timestamp: claims.milli_timestamp,
            changeset: claims.changeset,
            uid: claims.uid,
            user: claims.user,
            visible: claims.visible,
        })
    }
}
