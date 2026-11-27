//! Type-ordered frame delivery for indexed and forced repack.

use std::io::{BufReader, BufWriter, IntoInnerError, Read, Seek, Write};
use std::path::Path;

use crate::file_writer::FileWriter;
use crate::host::Entered;
use crate::scratch::{RunScratch, ScratchFile};
use crate::writer::PbfWriter;
use crate::{ElementKind, Error, Result};

struct Spool {
    file: BufWriter<ScratchFile>,
    frames: u64,
}

impl Spool {
    fn write(&mut self, frame: &[u8]) -> Result<()> {
        let len = u32::try_from(frame.len())
            .map_err(|_| Error::internal("repack spool frame length exceeds u32"))?;
        self.file.write_all(&len.to_le_bytes())?;
        self.file.write_all(frame)?;
        self.frames += 1;
        Ok(())
    }

    fn replay(
        self,
        writer: &mut PbfWriter<FileWriter>,
        invocation: &crate::host::Recorder,
    ) -> Result<()> {
        let mut file = self.file.into_inner().map_err(IntoInnerError::into_error)?;
        file.rewind()?;
        let mut reader = BufReader::new(file);
        for _ in 0..self.frames {
            invocation.check()?;
            let mut len = [0; 4];
            reader.read_exact(&mut len)?;
            let len = u32::from_le_bytes(len);
            if u64::from(len)
                > 4 + crate::blob::MAX_BLOB_HEADER_SIZE + crate::blob::MAX_BLOB_DATASIZE
            {
                return Err(Error::internal(
                    "repack spool frame length exceeds the PBF limit",
                ));
            }
            let mut frame = vec![0; len as usize];
            reader.read_exact(&mut frame)?;
            writer.write_raw_owned(frame)?;
        }
        Ok(())
    }
}

pub(super) struct FrameOutput<'w, 'a, 'h> {
    writer: &'w mut PbfWriter<FileWriter>,
    parent: Option<&'a Path>,
    host: &'a Entered<'h>,
    // These must close before the directory guard removes them.
    spools: [Option<Spool>; 2],
    scratch: Option<RunScratch>,
}

impl<'w, 'a, 'h> FrameOutput<'w, 'a, 'h> {
    pub fn new(
        writer: &'w mut PbfWriter<FileWriter>,
        parent: Option<&'a Path>,
        host: &'a Entered<'h>,
    ) -> Self {
        Self {
            writer,
            parent,
            host,
            spools: [None, None],
            scratch: None,
        }
    }

    pub fn write(&mut self, kind: ElementKind, frame: Vec<u8>) -> Result<()> {
        self.host.invocation.check()?;
        let Some(parent) = self.parent.filter(|_| kind != ElementKind::Node) else {
            return self.writer.write_raw_owned(frame);
        };
        let (index, name) = spool_slot(kind)?;
        let slot = &mut self.spools[index];
        if slot.is_none() {
            if self.scratch.is_none() {
                self.scratch = Some(RunScratch::create(parent, self.host)?);
            }
            let scratch = self
                .scratch
                .as_ref()
                .ok_or_else(|| Error::internal("missing repack scratch directory"))?;
            let path = scratch.file(name)?;
            let file = crate::scratch::create_read_write(&path, true)
                .map_err(|e| Error::from(e).context(format!("create {}", path.display())))?;
            *slot = Some(Spool {
                file: BufWriter::new(file),
                frames: 0,
            });
        }
        let spool = slot
            .as_mut()
            .ok_or_else(|| Error::internal("missing repack frame spool"))?;
        spool
            .write(&frame)
            .map_err(|e| e.context(format!("write {name}")))
    }

    pub fn replay(&mut self, kind: ElementKind) -> Result<()> {
        let (index, name) = spool_slot(kind)?;
        if let Some(spool) = self.spools[index].take() {
            spool
                .replay(self.writer, &self.host.invocation)
                .map_err(|e| e.context(format!("replay {name}")))?;
        }
        Ok(())
    }
}

fn spool_slot(kind: ElementKind) -> Result<(usize, &'static str)> {
    match kind {
        ElementKind::Way => Ok((0, "repack-ways.frames")),
        ElementKind::Relation => Ok((1, "repack-relations.frames")),
        ElementKind::Node => Err(Error::internal("repack attempted to spool node frames")),
    }
}
