//! An external sort of fixed-size records, for a verify step that must
//! order a stream sized by the data (`verify merge-changes`' simplify
//! model) without holding it.
//!
//! Records are buffered up to a fixed count, sorted and written as a run
//! file in the given directory; runs are merged at most [`FAN_IN`] at a
//! time, in further passes while more remain, so memory and open files are
//! bounded whatever the input's size; the last merge streams. A sort that
//! never filled its buffer never touches the disk. Run files are removed
//! once merged, inside the run-private directory.

#![allow(
    dead_code,
    reason = "shared verifier helpers consumed by the remaining family slices"
)]

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use crate::error::Error;

use crate::tunables::{SORT_CHUNK as CHUNK, SORT_FAN_IN as FAN_IN};

/// A record the sort can store: a fixed-size encoding and its order.
pub(crate) trait Record: Ord + Sized {
    const SIZE: usize;
    fn encode(&self, out: &mut [u8]);
    fn decode(bytes: &[u8]) -> Self;
}

pub(crate) struct Sorter<R: Record> {
    dir: PathBuf,
    name: String,
    chunk: Vec<R>,
    capacity: usize,
    runs: Vec<PathBuf>,
    next_run: usize,
}

impl<R: Record> Sorter<R> {
    /// A sorter whose run files are `dir/<name>.<n>.run`.
    pub(crate) fn new(dir: &Path, name: &str) -> Self {
        Self::with_capacity(dir, name, CHUNK)
    }

    fn with_capacity(dir: &Path, name: &str, capacity: usize) -> Self {
        Self {
            dir: dir.to_path_buf(),
            name: name.to_owned(),
            chunk: Vec::new(),
            capacity,
            runs: Vec::new(),
            next_run: 0,
        }
    }

    pub(crate) fn push(&mut self, record: R) -> Result<(), Error> {
        self.chunk.push(record);
        if self.chunk.len() >= self.capacity {
            self.spill()?;
        }
        Ok(())
    }

    fn run_path(&mut self) -> PathBuf {
        self.next_run += 1;
        self.dir
            .join(format!("{}.{}.run", self.name, self.next_run))
    }

    fn spill(&mut self) -> Result<(), Error> {
        let mut chunk = std::mem::take(&mut self.chunk);
        chunk.sort_unstable();
        let path = self.run_path();
        let mut writer = BufWriter::new(File::create(&path)?);
        let mut buf = vec![0; R::SIZE];
        for record in &chunk {
            record.encode(&mut buf);
            writer.write_all(&buf)?;
        }
        writer.flush()?;
        self.runs.push(path);
        Ok(())
    }

    /// Every record pushed, in order.
    pub(crate) fn finish(mut self) -> Result<Sorted<R>, Error> {
        if self.runs.is_empty() {
            let mut chunk = std::mem::take(&mut self.chunk);
            chunk.sort_unstable();
            chunk.reverse();
            return Ok(Sorted::Memory(chunk));
        }
        if !self.chunk.is_empty() {
            self.spill()?;
        }
        while self.runs.len() > FAN_IN {
            let batch: Vec<PathBuf> = self.runs.drain(..FAN_IN).collect();
            let path = self.run_path();
            let mut merge = Merge::<R>::open(batch)?;
            let mut writer = BufWriter::new(File::create(&path)?);
            let mut buf = vec![0; R::SIZE];
            while let Some(record) = merge.next_record()? {
                record.encode(&mut buf);
                writer.write_all(&buf)?;
            }
            writer.flush()?;
            merge.remove()?;
            self.runs.push(path);
        }
        Ok(Sorted::Merge(Merge::open(std::mem::take(&mut self.runs))?))
    }
}

/// The sorted records, read in order.
pub(crate) enum Sorted<R: Record> {
    /// Reversed, so popping yields ascending order.
    Memory(Vec<R>),
    Merge(Merge<R>),
}

impl<R: Record> Sorted<R> {
    pub(crate) fn next_record(&mut self) -> Result<Option<R>, Error> {
        match self {
            Self::Memory(records) => Ok(records.pop()),
            Self::Merge(merge) => merge.next_record(),
        }
    }

    /// Remove the run files the last merge reads.
    pub(crate) fn remove(self) -> Result<(), Error> {
        match self {
            Self::Memory(_) => Ok(()),
            Self::Merge(merge) => merge.remove(),
        }
    }
}

/// A k-way merge of sorted run files.
pub(crate) struct Merge<R: Record> {
    paths: Vec<PathBuf>,
    readers: Vec<BufReader<File>>,
    heap: BinaryHeap<Reverse<(R, usize)>>,
    buf: Vec<u8>,
}

impl<R: Record> Merge<R> {
    fn open(paths: Vec<PathBuf>) -> Result<Self, Error> {
        let mut merge = Self {
            readers: paths
                .iter()
                .map(|p| File::open(p).map(BufReader::new))
                .collect::<Result<_, _>>()?,
            paths,
            heap: BinaryHeap::new(),
            buf: vec![0; R::SIZE],
        };
        for i in 0..merge.readers.len() {
            merge.refill(i)?;
        }
        Ok(merge)
    }

    fn refill(&mut self, run: usize) -> Result<(), Error> {
        let reader = &mut self.readers[run];
        let mut filled = 0;
        while filled < R::SIZE {
            let read = reader.read(&mut self.buf[filled..])?;
            if read == 0 {
                break;
            }
            filled += read;
        }
        match filled {
            0 => Ok(()),
            n if n == R::SIZE => {
                self.heap.push(Reverse((R::decode(&self.buf), run)));
                Ok(())
            }
            _ => Err(Error::Failed(format!(
                "sort run {} ends inside a record",
                self.paths[run].display()
            ))),
        }
    }

    fn next_record(&mut self) -> Result<Option<R>, Error> {
        let Some(Reverse((record, run))) = self.heap.pop() else {
            return Ok(None);
        };
        self.refill(run)?;
        Ok(Some(record))
    }

    fn remove(self) -> Result<(), Error> {
        drop(self.readers);
        for path in &self.paths {
            std::fs::remove_file(path)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
    struct Pair(u32, u32);

    impl Record for Pair {
        const SIZE: usize = 8;
        fn encode(&self, out: &mut [u8]) {
            out[..4].copy_from_slice(&self.0.to_be_bytes());
            out[4..].copy_from_slice(&self.1.to_be_bytes());
        }
        fn decode(b: &[u8]) -> Self {
            Self(
                u32::from_be_bytes(b[..4].try_into().unwrap()),
                u32::from_be_bytes(b[4..].try_into().unwrap()),
            )
        }
    }

    fn sorted(capacity: usize, n: u32) -> Vec<Pair> {
        let scratch_dir = pbfhogg_test_scratch::scratch();
        let dir = scratch_dir.path().join("fixture");
        std::fs::create_dir_all(&dir).unwrap();
        let mut sorter = Sorter::with_capacity(&dir, "t", capacity);
        // A permutation with repeats in the first field.
        for i in 0..n {
            sorter.push(Pair((i * 7919) % 97, i)).unwrap();
        }
        let mut out = Vec::new();
        let mut s = sorter.finish().unwrap();
        while let Some(r) = s.next_record().unwrap() {
            out.push(r);
        }
        s.remove().unwrap();
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0, "runs removed");
        out
    }

    #[test]
    fn every_path_sorts_the_same_records() {
        let memory = sorted(1 << 20, 5000);
        assert_eq!(memory.len(), 5000);
        assert!(memory.windows(2).all(|w| w[0] <= w[1]));
        // One merge, and several passes past the fan-in (5000 / 10 runs).
        assert_eq!(sorted(1000, 5000), memory);
        assert_eq!(sorted(10, 5000), memory);
        assert!(sorted(10, 0).is_empty());
    }
}
