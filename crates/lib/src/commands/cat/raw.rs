//! Cat's header-first type filter with verbatim reads of retained frames.

use std::io;

use crate::error::Result;
use crate::file_reader::FileReader;
use crate::host::Entered;
use crate::read::frame::{FrameHeader, FrameSkip, FrameSource, Frames};
use crate::read::input::PinnedInput;

pub(super) struct RawInput {
    pub(super) frames: Frames<CatSource>,
}

impl RawInput {
    pub(super) fn open(
        input: &PinnedInput,
        direct_reads: bool,
        host: &Entered<'_>,
    ) -> Result<Self> {
        let reader = FileReader::open_in(input, direct_reads, &host.invocation)?;
        let mut frames = Frames::of_input(
            CatSource {
                reader,
                capture_header: false,
                header: Vec::new(),
            },
            input.path(),
            host,
        );
        // Cat repairs partial indexing by reading and reindexing the blobs
        // that have no readable claim.
        frames.accept_partial_index();
        Ok(Self { frames })
    }

    pub(super) fn next_header(&mut self) -> Result<Option<FrameHeader>> {
        let source = &mut self.frames.src;
        source.header.clear();
        source.capture_header = true;
        let result = self.frames.next_header();
        self.frames.src.capture_header = false;
        result
    }

    pub(super) fn read_frame(&mut self, header: &FrameHeader) -> Result<Vec<u8>> {
        let raw_header = &self.frames.src.header;
        assert_eq!(raw_header.len(), header.header_len);
        let header_len = u32::try_from(header.header_len)
            .map_err(|_| crate::Error::internal("cat frame header exceeds u32"))?;
        let mut frame = Vec::with_capacity(header.payload_offset() + header.data_size);
        frame.extend_from_slice(&header_len.to_be_bytes());
        frame.extend_from_slice(raw_header);
        self.frames.read_payload(&mut frame)?;
        Ok(frame)
    }
}

/// Capture the wire header during the walk's own exact read when it will
/// need to assemble a buffered frame. All other reads use FileReader's
/// zero-free append directly.
pub(super) struct CatSource {
    reader: FileReader,
    capture_header: bool,
    header: Vec<u8>,
}

impl FrameSource for CatSource {
    fn fill(&mut self, out: &mut [u8]) -> io::Result<usize> {
        self.reader.fill(out)
    }

    fn append(&mut self, n: usize, out: &mut Vec<u8>) -> io::Result<usize> {
        let start = out.len();
        let n = self.reader.append_to(n, out)?;
        if self.capture_header {
            self.header.extend_from_slice(&out[start..]);
        }
        Ok(n)
    }
}

impl FrameSkip for CatSource {
    fn skip_bytes(&mut self, n: u64) -> io::Result<bool> {
        self.reader.skip_bytes(n)
    }
}
