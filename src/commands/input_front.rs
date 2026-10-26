//! The front of a PBF input, read once: the leading OsmHeader and the
//! first-data-blob indexdata hint that a command's preconditions depend on.
//!
//! Before this existed, a command asked those questions through separate
//! opens - a `require_indexdata` probe, a reader for the header, another for
//! `is_sorted` - each re-reading the same prefix with its own failure path.
//! An [`InputFront`] is one open serving all of them, in whatever order the
//! caller asks, so a command's refusals keep the order its checks are
//! written in: the header is decoded only when asked for, and the indexdata
//! hint is read only when asked for.
//!
//! It is a precondition read, not a data path. The passes that follow open
//! their own descriptors: `posix_fadvise` advice and readahead state belong
//! to an open file description, so the header walker's `FADV_RANDOM` and a
//! buffered pass's `FADV_SEQUENTIAL` stay off the descriptions the other
//! passes read through. Handing this reader's file down to them would merge
//! that state. (Whether a merge would cost anything is unmeasured: the
//! classify schedules hand the walker's own descriptor to their worker
//! pools.) A command that pins its input opens this front, and every pass
//! after it, from the [`PinnedInput`](crate::read::input::PinnedInput), so
//! the separate descriptions are all of one file.

use std::path::Path;

use crate::HeaderBlock;
use crate::error::{Error, Result};
use crate::file_reader::FileReader;
use crate::frame::Frames;

/// Where a failure on the header frame sits: the input, blob 0, byte 0 (the
/// location the header readers this replaced stamped).
#[cold]
fn header_location(path: &Path) -> crate::error::Location {
    crate::error::Location::new(
        Some(&std::sync::Arc::from(path)),
        Some(0),
        Some(crate::ByteOffset(0)),
    )
}

/// `(is_sorted, has_indexdata)` of a PBF, from one front read: the sorted
/// flag from the decoded OsmHeader, then the first OsmData blob's indexdata
/// hint (a file with no data blob counts as indexed). A file without a
/// leading header is the missing-header error, ahead of either answer.
/// Partially indexed files surface at the consuming site, not here.
pub(crate) fn sorted_and_indexed<'a>(
    source: impl Into<crate::read::input::InputSource<'a>>,
    direct_io: bool,
    host: &crate::host::Host,
) -> Result<(bool, bool)> {
    let mut front = InputFront::open(source, direct_io, host)?;
    let sorted = front.is_sorted()?;
    let indexed = front.indexed()?;
    Ok((sorted, indexed))
}

/// See the module doc.
pub(crate) struct InputFront<'a> {
    path: &'a Path,
    /// Positioned after the header frame until the indexdata hint is read,
    /// then dropped, so the descriptor does not outlive the questions.
    frames: Option<Frames<FileReader>>,
    /// The header frame's Blob payload, until [`Self::header`] decodes it.
    header_payload: Vec<u8>,
    header: Option<HeaderBlock>,
    /// `(indexdata, tagdata)` of the first OsmData blob, once read.
    hint: Option<(bool, bool)>,
}

impl<'a> InputFront<'a> {
    /// Open `path` (O_DIRECT when `direct_io`, as the probes it replaces
    /// did) and read its header frame. A zero-byte file, or one whose first
    /// frame is not an OsmHeader, is the missing-header error, decided before
    /// any payload is read. The header payload is read here but decoded only
    /// by [`Self::header`].
    pub(crate) fn open(
        source: impl Into<crate::read::input::InputSource<'a>>,
        direct_io: bool,
        host: &crate::host::Host,
    ) -> Result<Self> {
        let source = source.into();
        let path = source.path();
        let mut frames = super::front_frames(
            FileReader::open(source, direct_io)?,
            path,
            &host.diagnostics,
            &host.cancel,
        );
        super::leading_header_frame(&mut frames, path)?;
        let mut header_payload = Vec::new();
        frames.read_payload(&mut header_payload)?;
        Ok(Self {
            path,
            frames: Some(frames),
            header_payload,
            header: None,
            hint: None,
        })
    }

    /// The decoded header, decoded on the first call.
    pub(crate) fn header(&mut self) -> Result<&HeaderBlock> {
        if self.header.is_none() {
            let header = crate::blob::decode_blob_to_headerblock(&self.header_payload)
                .map_err(|e| e.locate(header_location(self.path)))?;
            self.header_payload = Vec::new();
            self.header = Some(header);
        }
        self.header
            .as_ref()
            .ok_or_else(|| Error::internal("input front: header slot empty after decode"))
    }

    /// The decoded header, owned; ends the front.
    pub(crate) fn into_header(mut self) -> Result<HeaderBlock> {
        self.header()?;
        self.header
            .take()
            .ok_or_else(|| Error::internal("input front: header slot empty after decode"))
    }

    /// Whether the header declares `Sort.Type_then_ID`.
    pub(crate) fn is_sorted(&mut self) -> Result<bool> {
        Ok(self.header()?.is_sorted())
    }

    /// Whether the first OsmData blob carries indexdata, the hint
    /// [`super::has_indexdata`] answers (a file with no data blob counts as
    /// indexed). Read on the first call, which releases the descriptor.
    pub(crate) fn indexed(&mut self) -> Result<bool> {
        if let Some((indexed, _)) = self.hint {
            return Ok(indexed);
        }
        let mut frames = self
            .frames
            .take()
            .ok_or_else(|| Error::internal("input front: reader released before the hint"))?;
        let hint = super::probe_to_first_data_blob(&mut frames)?;
        self.hint = Some(hint);
        Ok(hint.0)
    }

    /// [`super::require_indexdata`] on this front's hint: the same policy,
    /// reason and forced-run Note, without another open.
    pub(crate) fn require_indexdata(
        &mut self,
        force: bool,
        reason: &str,
        host: &crate::host::Host,
    ) -> Result<bool> {
        let present = self.indexed()?;
        super::indexdata_policy(present, self.path, force, reason, host)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::InputFront;
    use crate::ErrorKind;

    /// One frame: `[len][BlobHeader{type, datasize}][Blob{raw: payload}]`.
    fn frame(kind: &str, payload: &[u8]) -> Vec<u8> {
        let mut blob = vec![0x0A, u8::try_from(payload.len()).unwrap()];
        blob.extend_from_slice(payload);
        let mut header = vec![0x0A, u8::try_from(kind.len()).unwrap()];
        header.extend_from_slice(kind.as_bytes());
        header.extend_from_slice(&[0x18, u8::try_from(blob.len()).unwrap()]);
        let mut out = u32::try_from(header.len()).unwrap().to_be_bytes().to_vec();
        out.extend_from_slice(&header);
        out.extend_from_slice(&blob);
        out
    }

    /// The front keeps the order its caller asks in: on a file whose header
    /// payload does not decode and whose first data blob has no indexdata,
    /// the indexdata refusal (or the forced pass) comes first and the header
    /// failure only when the header is asked for.
    #[test]
    fn indexdata_question_precedes_header_decode() {
        let dir = crate::path_guard::tests::scratch();
        let path = dir.path().join("front.osm.pbf");
        let mut bytes = frame("OSMHeader", &[0xFF, 0xFF, 0xFF]);
        bytes.extend_from_slice(&frame("OSMData", &[0x00]));
        std::fs::write(&path, bytes).unwrap();
        let host = crate::host::Host::for_tests();

        let mut front = InputFront::open(&path, false, &host).unwrap();
        let refused = front
            .require_indexdata(false, "test reason", &host)
            .unwrap_err();
        assert!(
            matches!(refused.kind(), ErrorKind::MissingIndexdata { .. }),
            "{refused:?}"
        );

        let mut forced = InputFront::open(&path, false, &host).unwrap();
        assert!(
            !forced
                .require_indexdata(true, "test reason", &host)
                .unwrap()
        );
        assert!(forced.header().is_err());
    }

    /// A header-only file is indexed to the front, as to `has_indexdata`;
    /// a file opening with a data blob is the missing-header error.
    #[test]
    fn header_only_is_indexed_and_headerless_is_missing_header() {
        let dir = crate::path_guard::tests::scratch();
        let host = crate::host::Host::for_tests();
        let header_only = dir.path().join("header-only.osm.pbf");
        std::fs::write(&header_only, frame("OSMHeader", &[])).unwrap();
        let mut front = InputFront::open(&header_only, false, &host).unwrap();
        assert!(front.indexed().unwrap());
        assert!(!front.is_sorted().unwrap());

        let headerless = dir.path().join("headerless.osm.pbf");
        std::fs::write(&headerless, frame("OSMData", &[0x00])).unwrap();
        let err = InputFront::open(&headerless, false, &host).err().unwrap();
        assert!(matches!(err.kind(), ErrorKind::MissingHeader), "{err:?}");
    }
}
