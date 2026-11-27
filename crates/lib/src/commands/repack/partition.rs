//! Ordered grow planning. A source is charged once, including when a cut
//! leaves slices in two output blocks or several kind lanes use it.

use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use rayon::prelude::*;

use crate::read::input::InputFile;
use crate::scan::classify::{ScheduledBlob, parallel_project_phase_ordered};
use crate::tunables::{
    DECOMPRESS_POOL_MAX_RETAINED_CAPACITY, REPACK_DECODE_WINDOW, REPACK_GROW_BATCH_BLOCKS,
    REPACK_RETAINED_SOURCE_BYTES,
};
use crate::{ElementKind, Error, PrimitiveBlock, Result};

use super::encode::{self, Config, Decoded, WorkerScratch};
use super::spool::FrameOutput;
use super::{KINDS, PhaseStats};

struct Source {
    block: PrimitiveBlock,
    entry: ScheduledBlob,
    bytes: usize,
    retained: Arc<AtomicUsize>,
}

impl Source {
    fn weight(block: &PrimitiveBlock) -> usize {
        // A PrimitiveBlock appends two u32s per string and group to the
        // decoded bytes. Include the source and its maximum six slices
        // (two per kind). The decoder separately limits recycled capacity.
        block.decompressed_size()
            + (block.string_table_len() + block.groups().len()) * size_of::<(u32, u32)>()
            + size_of::<Self>()
            + 2 * KINDS.len() * size_of::<SourceSlice>()
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        self.retained.fetch_sub(self.bytes, Ordering::Relaxed);
    }
}

struct SourceSlice {
    source: Arc<Source>,
    range: Range<usize>,
}

#[derive(Default)]
struct Partition {
    len: usize,
    slices: Vec<SourceSlice>,
    // A cap can cover arbitrarily many source bytes. If an unfinished
    // partition consumes the admission budget, a worker encodes its
    // buffered prefix and gives back the builder with the block still
    // open. Later slices continue the same block without a new cut.
    prefix: Option<Box<WorkerScratch>>,
}

impl Partition {
    fn evacuate(&mut self, kind: ElementKind, config: &Config<'_, '_>) -> Result<()> {
        if self.slices.is_empty() {
            return Ok(());
        }
        let worker = self.prefix.get_or_insert_with(|| Box::new(config.worker()));
        let mut frames = config.frames();
        add_slices(
            worker,
            kind,
            std::mem::take(&mut self.slices),
            config,
            &mut frames,
        )?;
        worker.release_scratch();
        if !frames.frames.is_empty() {
            return Err(Error::internal(
                "repack emitted an unfinished grow partition",
            ));
        }
        Ok(())
    }

    fn encode(
        self,
        kind: ElementKind,
        worker: &mut WorkerScratch,
        config: &Config<'_, '_>,
    ) -> Result<Vec<u8>> {
        let mut prefix = self.prefix;
        let worker = if let Some(encoded) = prefix.as_deref_mut() {
            encoded
        } else {
            worker.start_partition()?;
            worker
        };
        let mut frames = config.frames();
        add_slices(worker, kind, self.slices, config, &mut frames)?;
        worker.finish(&mut frames)?;
        match frames.frames.pop() {
            Some((actual, frame)) if actual == kind && frames.frames.is_empty() => Ok(frame),
            _ => Err(Error::internal(
                "repack grow partition did not make one frame of its kind",
            )),
        }
    }
}

fn add_slices(
    worker: &mut WorkerScratch,
    kind: ElementKind,
    slices: Vec<SourceSlice>,
    config: &Config<'_, '_>,
    frames: &mut crate::commands::FramedBlocks<'_>,
) -> Result<()> {
    for SourceSlice { source, range } in slices {
        config.host.invocation.check()?;
        worker
            .add_slice(
                &source.block,
                kind,
                range,
                config.preserve_locations,
                frames,
            )
            .map_err(|e| e.locate_blob(source.entry.location(config.input_path)))?;
    }
    Ok(())
}

#[derive(Default)]
struct KindPlan {
    open: Partition,
    stats: PhaseStats,
    // The old grow builder only flushed a full block on the next add.
    // Preserve the coalesces counter at that exact-cap boundary as well.
    in_grow_run: bool,
}

pub(super) struct Planner<'p, 'a, 'h, 'w> {
    config: &'p Config<'a, 'h>,
    output: &'p mut FrameOutput<'w, 'a, 'h>,
    kinds: [KindPlan; 3],
    ready: Vec<(ElementKind, Partition)>,
    retained: Arc<AtomicUsize>,
    budget: usize,
}

impl<'p, 'a, 'h, 'w> Planner<'p, 'a, 'h, 'w> {
    pub fn new(config: &'p Config<'a, 'h>, output: &'p mut FrameOutput<'w, 'a, 'h>) -> Self {
        Self {
            config,
            output,
            kinds: std::array::from_fn(|_| KindPlan::default()),
            ready: Vec::new(),
            retained: Arc::default(),
            budget: REPACK_RETAINED_SOURCE_BYTES,
        }
    }

    pub fn scan(&mut self, file: &InputFile, schedule: &[ScheduledBlob]) -> Result<()> {
        let config = self.config;
        parallel_project_phase_ordered(
            file,
            schedule,
            None,
            Some(REPACK_DECODE_WINDOW),
            || config.worker(),
            |buffer, worker| encode::decode(buffer.block()?, worker, config),
            |position, decoded| self.ingest(schedule[position], decoded),
            config.host,
        )
    }

    fn ingest(&mut self, entry: ScheduledBlob, decoded: Decoded) -> Result<()> {
        let source = if let Some(block) = decoded.source {
            let bytes = Source::weight(&block);
            if self.retained_bytes().saturating_add(bytes) > self.budget {
                self.drain()?;
                if self.retained_bytes().saturating_add(bytes) > self.budget {
                    self.evacuate()?;
                }
            }
            self.retained.fetch_add(bytes, Ordering::Relaxed);
            Some(Arc::new(Source {
                block,
                entry,
                bytes,
                retained: Arc::clone(&self.retained),
            }))
        } else {
            None
        };

        for ((kind, count), frames) in KINDS.into_iter().zip(decoded.counts).zip(decoded.shrink) {
            if count == 0 {
                continue;
            }
            let cap = self.config.cap.get();
            let plan = &mut self.kinds[usize::from(kind.ordinal())];
            plan.stats.elements += count as u64;
            if count >= cap {
                plan.in_grow_run = false;
                plan.stats.worker_full_blobs += (count / cap) as u64;
                plan.stats.worker_tail_blobs += u64::from(count % cap != 0);
                self.seal(kind);
                self.drain()?;
                for (actual, frame) in frames {
                    if actual != kind {
                        return Err(Error::internal("repack shrink frame has the wrong kind"));
                    }
                    self.output.write(kind, frame)?;
                }
            } else {
                let source = source
                    .as_ref()
                    .ok_or_else(|| Error::internal("missing grow source"))?;
                plan.stats.coalesces += u64::from(plan.in_grow_run);
                plan.in_grow_run = true;
                self.grow(kind, source, count);
            }
        }

        drop(source);
        if self.retained_bytes() > self.budget || self.has_large_tail() {
            self.drain()?;
            self.evacuate()?;
        } else if self.ready.len() >= REPACK_GROW_BATCH_BLOCKS {
            self.drain()?;
        }
        Ok(())
    }

    fn retained_bytes(&self) -> usize {
        self.retained.load(Ordering::Relaxed)
    }

    fn has_large_tail(&self) -> bool {
        self.kinds.iter().any(|plan| {
            plan.open.slices.last().is_some_and(|slice| {
                slice.source.bytes > DECOMPRESS_POOL_MAX_RETAINED_CAPACITY
                    && slice.range.len() <= slice.range.start
            })
        })
    }

    fn grow(&mut self, kind: ElementKind, source: &Arc<Source>, count: usize) {
        let index = usize::from(kind.ordinal());
        let cap = self.config.cap.get();
        let mut start = 0;
        while start < count {
            let partition = &mut self.kinds[index].open;
            let len = (count - start).min(cap - partition.len);
            partition.slices.push(SourceSlice {
                source: Arc::clone(source),
                range: start..start + len,
            });
            partition.len += len;
            start += len;
            if partition.len == cap {
                self.seal(kind);
            }
        }
    }

    fn seal(&mut self, kind: ElementKind) {
        let plan = &mut self.kinds[usize::from(kind.ordinal())];
        if plan.open.len != 0 {
            self.ready.push((kind, std::mem::take(&mut plan.open)));
            plan.stats.grow_blobs += 1;
        }
    }

    fn evacuate(&mut self) -> Result<()> {
        let config = self.config;
        // Work on the pending partitions only. All their source slices
        // are dropped before admission resumes.
        let kinds = &mut self.kinds;
        let results: Vec<Result<()>> = config.host.executor.install(|| {
            kinds
                .par_iter_mut()
                .zip(KINDS)
                .map(|(plan, kind)| plan.open.evacuate(kind, config))
                .collect()
        });
        for result in results {
            result?;
        }
        if self.retained_bytes() != 0 {
            return Err(Error::internal(
                "repack retained a source after evacuating its slices",
            ));
        }
        Ok(())
    }

    fn drain(&mut self) -> Result<()> {
        if self.ready.is_empty() {
            return Ok(());
        }
        let config = self.config;
        let ready = std::mem::take(&mut self.ready);
        let results: Vec<_> = config.host.executor.install(|| {
            ready
                .into_par_iter()
                .map_init(
                    || config.worker(),
                    |worker, (kind, partition)| (kind, partition.encode(kind, worker, config)),
                )
                .collect()
        });
        for (kind, result) in results {
            self.output.write(kind, result?)?;
        }
        Ok(())
    }

    pub fn finish(mut self) -> Result<[PhaseStats; 3]> {
        for kind in KINDS {
            self.seal(kind);
        }
        if self.ready.len() == 1 {
            self.frame_final_partition()?;
        } else {
            self.drain()?;
        }
        if self.retained_bytes() != 0 {
            return Err(Error::internal(
                "repack retained a source after its final partition",
            ));
        }
        Ok(self.kinds.map(|plan| plan.stats))
    }

    /// A shrink-heavy phase often has just one grow remainder. Starting
    /// the ambient executor after the decode pool exited can strand that
    /// pool's allocator arenas on idle threads for the next phase. Give
    /// this single partition a worker that joins before the phase ends.
    fn frame_final_partition(&mut self) -> Result<()> {
        let (kind, partition) = self
            .ready
            .pop()
            .ok_or_else(|| Error::internal("missing final repack partition"))?;
        let config = self.config;
        let frame = crate::threads::scope(&config.host.invocation, |scope| {
            let worker = scope
                .spawn(move || partition.encode(kind, &mut config.worker(), config))
                .map_err(|e| Error::from(e).context("spawn repack partition worker"))?;
            match worker.join() {
                Ok(result) => result,
                Err(panic) => std::panic::resume_unwind(panic),
            }
        })?;
        self.output.write(kind, frame)
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;
    use std::path::Path;

    use super::*;
    use crate::block_builder::{BlockBuilder, HeaderBuilder, OwnedBlock};
    use crate::host::Host;
    use crate::output::{Output, Overwrite};
    use crate::scan::classify::KindClaims;
    use crate::write::OutputBackend;
    use crate::writer::{Compression, PbfWriter};

    fn nodes(ids: Range<i64>) -> Result<OwnedBlock> {
        let mut builder = BlockBuilder::new();
        for id in ids {
            builder.add_node(id, 10, -21, [("key", "value")], None);
        }
        builder
            .take_owned()?
            .ok_or_else(|| Error::internal("empty repack source fixture"))
    }

    #[test]
    fn grow_budget_releases_a_prefix_and_counts_shared_slices_once() -> Result<()> {
        let directory = pbfhogg_test_scratch::scratch();
        let path = directory.path().join("output.osm.pbf");
        let host = Host::for_tests();
        let entered = host.entered();
        let target =
            Output::new(&path, Overwrite::Refuse, &host.diagnostics)?.bind(&host.invocation)?;
        let header = HeaderBuilder::new().build()?;
        let mut writer =
            target.pbf_writer(Compression::None, &header, OutputBackend::Buffered, &host)?;
        let mut output = FrameOutput::new(&mut writer, None, &entered);
        let cap = NonZeroUsize::MIN.saturating_add(4);
        let input_path = Arc::from(Path::new("input.osm.pbf"));
        let config = Config {
            cap,
            prealloc_bound: 5,
            compression: Compression::None,
            preserve_locations: false,
            host: &entered,
            input_path: &input_path,
        };
        let mut plan = Planner::new(&config, &mut output);
        let first = PrimitiveBlock::from_vec(nodes(1..3)?.bytes)?;
        let second = PrimitiveBlock::from_vec(nodes(3..7)?.bytes)?;
        let second_bytes = Source::weight(&second);
        plan.budget = second_bytes.max(Source::weight(&first));
        let entry = ScheduledBlob::at_frame(0, 4, 1, None, KindClaims::Refuse)?;
        let mut worker = config.worker();
        plan.ingest(entry, encode::decode(first, &mut worker, &config)?)?;
        let previous = Arc::downgrade(&plan.kinds[0].open.slices[0].source);
        plan.ingest(entry, encode::decode(second, &mut worker, &config)?)?;

        assert!(previous.upgrade().is_none());
        assert!(plan.ready[0].1.prefix.is_some());
        assert_eq!(plan.retained_bytes(), second_bytes);
        assert_eq!(plan.kinds[0].open.slices[0].range, 3..4);
        plan.drain()?;
        assert_eq!(plan.retained_bytes(), second_bytes);
        assert_eq!(plan.finish()?[0].grow_blobs, 2);
        writer.flush()?;
        drop(writer);
        target.publish()?;

        let mut expected = PbfWriter::new(Vec::new(), Compression::None);
        expected.write_header(&header)?;
        expected.write_primitive_block_owned(nodes(1..6)?)?;
        expected.write_primitive_block_owned(nodes(6..7)?)?;
        expected.flush()?;
        assert_eq!(std::fs::read(path)?, expected.into_inner()?);
        Ok(())
    }
}
