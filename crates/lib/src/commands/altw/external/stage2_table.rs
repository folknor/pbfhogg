//! The stage 2 lookup join. Stage 1's shard layout stays fixed. A worker
//! subdivides its bucket only if neither table form fits the byte budget.

use std::fs::File;
use std::io::Read;
use std::sync::atomic::Ordering::Relaxed;

use super::super::{ID_WORD_SIZE, keys::KeySpan};
use super::{
    Error, Family, NodeBlobInfo, NodeTuple, Result, ShardCodec, ShardRecord, Stage2Counters,
    Stage2Job, WalkOut, WorkerBufs, check_duplicate, emit_resolved, flush_bucket, read_node_blob,
    record_max,
};
use crate::blob_meta::ClaimWalk;
use crate::coord_pack::PRESENT;
use crate::counters::Stopwatch;
use crate::tunables::ALTW_STAGE2_SHARD_READ_BYTES;

const SLOT_BYTES: usize = size_of::<u64>();
const RANK_BYTES: usize = SLOT_BYTES + size_of::<u64>();
const _: () = assert!(
    ALTW_STAGE2_SHARD_READ_BYTES != 0
        && ALTW_STAGE2_SHARD_READ_BYTES.is_multiple_of(2 * ID_WORD_SIZE)
);
/// A node whose first out-of-domain or conflicting coordinate must be
/// replayed if a way references it. Reserving one flag avoids allocating
/// error storage proportional to the number of malformed nodes.
const BAD_COORD: u64 = PRESENT;

#[derive(Clone, Copy, Debug)]
struct Partition {
    /// Bounds in one family, after ID-to-key conversion. A mixed-sign
    /// blob can feed both families, but their offsets are independent.
    lo: u64,
    hi: u64,
    /// Number of key slots to reserve for a rank table. None for dense.
    rank_slots: Option<usize>,
}

impl Partition {
    /// `spans` starts at the first blob reaching `lo`; `end` is exclusive.
    /// The physical node count and the number of IDs inside the
    /// intersection both bound the number of distinct keys per span.
    fn fit(lo: u64, end: u64, spans: &[KeySpan], budget: usize) -> Self {
        let dense_limit = (budget / SLOT_BYTES) as u64;
        let rank_limit = (budget / RANK_BYTES) as u64;
        let rank_bound = |hi: u64| {
            spans
                .iter()
                .take_while(|s| s.lo < hi)
                .map(|s| s.count.min((s.hi + 1).min(hi) - s.lo.max(lo)))
                .fold(0u64, u64::saturating_add)
        };
        let dense_hi = end.min(lo + dense_limit);
        let hi = if dense_hi == end || rank_bound(end) <= rank_limit {
            end
        } else if rank_bound(dense_hi) > rank_limit {
            dense_hi
        } else {
            // Find the largest prefix whose rank table fits. This also
            // skips over gaps in a wide bucket without iterating empty IDs.
            let (mut fits, mut too_wide) = (dense_hi, end);
            while too_wide - fits > 1 {
                let mid = fits + (too_wide - fits) / 2;
                if rank_bound(mid) <= rank_limit {
                    fits = mid;
                } else {
                    too_wide = mid;
                }
            }
            fits
        };
        let distinct_bound = rank_bound(hi);
        let width = hi - lo;
        // Either width <= dense_limit or distinct_bound <= rank_limit.
        let dense = width <= dense_limit && width <= distinct_bound.saturating_mul(2);
        #[allow(clippy::cast_possible_truncation)]
        let rank_slots = (!dense).then_some(distinct_bound as usize);
        Self { lo, hi, rank_slots }
    }
}

#[derive(Default)]
pub(super) enum NodeTable {
    #[default]
    Empty,
    Dense(Vec<u64>),
    Ranked {
        keys: Vec<u64>,
        slots: Vec<u64>,
    },
}

impl NodeTable {
    fn reset(&mut self, partition: Partition) {
        if let Some(limit) = partition.rank_slots {
            if let Self::Ranked { keys, slots } = self
                && keys.capacity().min(slots.capacity()) >= limit
            {
                keys.clear();
                slots.clear();
                return;
            }
            *self = Self::Empty;
            *self = Self::Ranked {
                keys: Vec::with_capacity(limit),
                slots: Vec::with_capacity(limit),
            };
        } else {
            // fit chose dense only when its width fits the usize budget.
            #[allow(clippy::cast_possible_truncation)]
            let width = (partition.hi - partition.lo) as usize;
            if let Self::Dense(slots) = self
                && slots.capacity() >= width
            {
                slots.clear();
                slots.resize(width, 0);
                return;
            }
            *self = Self::Empty;
            *self = Self::Dense(vec![0; width]);
        }
    }

    fn bytes(&self) -> usize {
        match self {
            Self::Empty => 0,
            Self::Dense(slots) => slots.capacity() * SLOT_BYTES,
            Self::Ranked { keys, slots } => {
                keys.capacity() * size_of::<u64>() + slots.capacity() * SLOT_BYTES
            }
        }
    }

    /// Population follows canonical key order. A duplicate can only update
    /// the slot already assigned to its key, even across blob boundaries.
    #[inline]
    fn put(&mut self, partition: Partition, key: u64, lat: i32, lon: i32) -> bool {
        let Some(local) = key.checked_sub(partition.lo) else {
            return true;
        };
        if key >= partition.hi {
            return true;
        }

        // The low 31 bits encode latitude plus one after biasing. Zero
        // denotes absence; bit 31 is reserved for the exceptional state.
        let packed =
            crate::coord_pack::pack(lat, lon).map_or(BAD_COORD, |word| (word ^ PRESENT) + 1);
        match self {
            Self::Dense(slots) => {
                // The partition width fits usize by construction.
                #[allow(clippy::cast_possible_truncation)]
                let slot = &mut slots[local as usize];
                merge_slot(slot, packed);
            }
            Self::Ranked { keys, slots } => {
                if let Some(&last) = keys.last() {
                    if key == last {
                        merge_slot(&mut slots[keys.len() - 1], packed);
                        return true;
                    }
                    if key < last {
                        return false;
                    }
                }
                if partition.rank_slots == Some(keys.len()) {
                    return false;
                }
                keys.push(key);
                slots.push(packed);
            }
            Self::Empty => return false,
        }
        true
    }

    #[inline]
    fn get(&self, partition: Partition, key: u64) -> u64 {
        if key < partition.lo || key >= partition.hi {
            return 0;
        }
        match self {
            Self::Empty => 0,
            Self::Dense(slots) => {
                #[allow(clippy::cast_possible_truncation)]
                let offset = (key - partition.lo) as usize;
                slots[offset]
            }
            Self::Ranked { keys, slots } => keys.binary_search(&key).map_or(0, |rank| slots[rank]),
        }
    }
}

#[inline]
fn merge_slot(slot: &mut u64, packed: u64) {
    if *slot == 0 {
        *slot = packed;
    } else if *slot != packed {
        *slot = BAD_COORD;
    }
}

/// The table marks bad coordinates without refusing them. Only a reference
/// to the marked key triggers replay, preserving the sorted walk's rules
/// for the first coordinate and for each further duplicate in input order.
#[cold]
fn replay_bad_coord<R>(
    job: &Stage2Job<'_, '_>,
    c: &Stage2Counters,
    pbf_file: &File,
    w: &mut WorkerBufs<R>,
    spans: &[KeySpan],
    family: Family,
    key: u64,
) -> Result<()> {
    let id = family.id_of(key);
    let mut first = None;
    for span in spans.iter().filter(|s| s.lo <= key && key <= s.hi) {
        let blob = get_blob(job, span)?;
        read_node_blob(
            job,
            c,
            pbf_file,
            blob,
            &mut w.node_read_buf,
            &mut w.node_decompress_buf,
        )?;
        // The first coordinate carries across blobs. A refusal from this
        // blob is returned only after its scanner and claim checks pass.
        let mut failure = Ok(());
        scan_nodes(
            job,
            blob,
            &w.node_decompress_buf,
            &mut w.node_group_starts,
            |seen, lat, lon| {
                if seen != id || failure.is_err() {
                    return;
                }
                let first = *first.get_or_insert((key, lat, lon));
                failure = check_duplicate(family, first, &NodeTuple { id, lat, lon });
            },
        )?;
        failure?;
    }
    Err(Error::internal(
        "stage2: marked coordinate passed duplicate replay",
    ))
}

fn get_blob<'a>(job: &Stage2Job<'a, '_>, span: &KeySpan) -> Result<&'a NodeBlobInfo> {
    job.node_blob_mapping
        .get(span.blob as usize)
        .ok_or_else(|| Error::internal("stage2: a key span names no node blob"))
}

/// Populate from the scanner while checking the whole blob in canonical
/// order, including nodes assigned to a different partition or family.
fn scan_nodes(
    job: &Stage2Job<'_, '_>,
    blob: &NodeBlobInfo,
    bytes: &[u8],
    groups: &mut Vec<(usize, usize)>,
    mut visit: impl FnMut(i64, i32, i32),
) -> Result<()> {
    let claim = blob.claim();
    let in_input = |e: Error| job.input_pbf.locate_at(blob.data_offset, e);
    let mut walk = ClaimWalk::new(Some(&claim), super::super::blob_meta::EXTERNAL_ORDER);
    let mut order = Ok(());
    let kinds = crate::scan::node::extract_node_coords(bytes, groups, |id, lat, lon| {
        if order.is_ok() {
            order = walk.observe(id);
        }
        visit(id, lat, lon);
    })
    .map_err(|e| in_input(e.context("stage2 node extract")))?;
    kinds
        .refuse_under(Some(crate::ElementKind::Node))
        .map_err(in_input)?;
    order.map_err(in_input)?;
    walk.finish().map_err(in_input)?;
    Ok(())
}

#[derive(Default)]
struct NodeReads {
    blobs: usize,
    straddlers: u64,
    buffer_peak: usize,
}

impl NodeReads {
    #[allow(clippy::too_many_arguments)]
    fn fill<R>(
        &mut self,
        job: &Stage2Job<'_, '_>,
        c: &Stage2Counters,
        pbf_file: &File,
        w: &mut WorkerBufs<R>,
        spans: &[KeySpan],
        family: Family,
        partition: Partition,
    ) -> Result<()> {
        w.table.reset(partition);
        for span in spans {
            let blob = get_blob(job, span)?;
            read_node_blob(
                job,
                c,
                pbf_file,
                blob,
                &mut w.node_read_buf,
                &mut w.node_decompress_buf,
            )?;
            let t_extract = Stopwatch::start();
            let table = &mut w.table;
            let mut fits = true;
            // Filter by family after feeding the claim check, which
            // validates the whole blob even across the sign boundary.
            scan_nodes(
                job,
                blob,
                &w.node_decompress_buf,
                &mut w.node_group_starts,
                |id, lat, lon| {
                    let (node_family, key) = job.layout.family_key(id);
                    if node_family == family {
                        fits &= table.put(partition, key, lat, lon);
                    }
                },
            )?;
            if !fits {
                return Err(Error::internal(
                    "stage2: node keys did not fit their partition",
                ));
            }
            t_extract.record_ms(&c.node_extract_ms);
            t_extract.record_ns(&c.node_extract_ns);
            self.blobs += 1;
            self.straddlers += u64::from(span.lo < partition.lo || span.hi >= partition.hi);
            self.buffer_peak = self.buffer_peak.max(
                w.table.bytes()
                    + w.node_read_buf.capacity()
                    + w.node_decompress_buf.capacity()
                    + w.node_group_starts.capacity() * size_of::<(usize, usize)>(),
            );
            w.node_read_buf.release_outlier(&job.host.invocation);
            crate::blob::release_decode_outlier(&mut w.node_decompress_buf, &job.host.invocation);
        }
        Ok(())
    }
}

struct Requests<'a, 'j, 'h> {
    job: &'a Stage2Job<'j, 'h>,
    counters: &'a Stage2Counters,
    bucket: usize,
}

impl Requests<'_, '_, '_> {
    fn visit<C: ShardCodec>(
        &self,
        codec: C,
        data: &mut Vec<u8>,
        final_partition: bool,
        mut visit: impl FnMut(C::Record) -> Result<()>,
    ) -> Result<()> {
        let mut seen = 0u64;
        let c = self.counters;
        let t_load = Stopwatch::start();
        // Each shard is visited in the same order stage 1 wrote it.
        for worker in 0..self.job.num_shard_workers {
            let path = super::super::radix::id_shard_path(self.job.scratch, worker, self.bucket)?;
            let named = |what: &str, e: std::io::Error| {
                Error::from(e).context(format!("failed to {what} id shard {}", path.display()))
            };
            let mut file = match crate::scratch::open(&path) {
                Ok(file) => file,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(named("open", e)),
            };
            c.open_calls.fetch_add(1, Relaxed);
            c.stat_calls.fetch_add(1, Relaxed);
            let len = file.metadata().map_err(|e| named("stat", e))?.len();
            if len == 0 {
                continue;
            }
            self.read_shard(codec, &mut file, len, data, |record| {
                seen += 1;
                visit(record)
            })
            .map_err(|e| e.context(format!("id shard {}", path.display())))?;
            if final_partition && crate::fadvise::evict_scratch(&file, &self.job.host.invocation) {
                c.fadvise_calls.fetch_add(1, Relaxed);
                c.fadvise_bytes.fetch_add(len, Relaxed);
            }
        }
        let expected = self.job.id_bucket_counts[self.bucket];
        if seen != expected {
            return Err(Error::internal(format!(
                "stage2 bucket {}: id shards hold {seen} records, pass A wrote {expected}",
                self.bucket,
            )));
        }
        t_load.record_ms(&c.request_scan_ms);
        c.request_scans.fetch_add(1, Relaxed);
        Ok(())
    }

    fn read_shard<C: ShardCodec>(
        &self,
        codec: C,
        mut reader: impl Read,
        mut remaining: u64,
        data: &mut Vec<u8>,
        mut visit: impl FnMut(C::Record) -> Result<()>,
    ) -> Result<()> {
        // Copy at most one pending word to the front between reads. Never
        // let a dangling escape borrow a word from the next shard.
        data.clear();
        let max_batch = ALTW_STAGE2_SHARD_READ_BYTES + ID_WORD_SIZE;
        #[allow(clippy::cast_possible_truncation)]
        data.reserve_exact(remaining.min(max_batch as u64) as usize);
        let mut carry = 0;
        while remaining != 0 {
            self.job.host.invocation.check()?;
            #[allow(clippy::cast_possible_truncation)]
            let count = remaining.min(ALTW_STAGE2_SHARD_READ_BYTES as u64) as usize;
            remaining -= count as u64;
            data.resize(carry + count, 0);
            reader.read_exact(&mut data[carry..])?;
            let used = codec.visit_records(data, remaining == 0, &mut visit)?;
            data.copy_within(used.., 0);
            carry = data.len() - used;
        }
        Ok(())
    }
}

pub(super) fn join_bucket<C: ShardCodec>(
    codec: C,
    job: &Stage2Job<'_, '_>,
    c: &Stage2Counters,
    pbf_file: &File,
    bucket: usize,
    w: &mut WorkerBufs<C::Record>,
    budget: usize,
) -> Result<()> {
    job.host.invocation.check()?;
    if budget < SLOT_BYTES {
        return Err(Error::internal(
            "stage2: table budget cannot hold one coordinate",
        ));
    }
    let Some((family, lo, hi)) = job.layout.bucket(bucket) else {
        return Err(Error::internal(format!(
            "stage2: id bucket {bucket} holds records but no key routes to it"
        )));
    };
    let all_spans = job.node_spans.of(family);
    let mut spans = &all_spans
        [all_spans.partition_point(|s| s.hi < lo)..all_spans.partition_point(|s| s.lo < hi)];
    let requests = Requests {
        job,
        counters: c,
        bucket,
    };
    let t_resolve = Stopwatch::start();
    let resolved_before = w.local_resolved;
    let mut reads = NodeReads::default();

    // Even a bucket with no overlapping node blobs must verify the shard
    // files and their record count. Its requests are all orphans.
    if spans.is_empty() {
        requests.visit(codec, &mut w.loader.data_buf, true, |_| Ok(()))?;
    }
    let mut next = lo;
    while let Some(first) = spans.first() {
        let start = next.max(first.lo);
        let end = hi.min(spans.last().map_or(start, |s| s.hi + 1));
        let partition = Partition::fit(start, end, spans, budget);
        let partitions = if partition.rank_slots.is_some() {
            &c.ranked_partitions
        } else {
            &c.dense_partitions
        };
        partitions.fetch_add(1, Relaxed);
        let touched = &spans[..spans.partition_point(|s| s.lo < partition.hi)];
        let rest = if partition.hi == end {
            &spans[spans.len()..]
        } else {
            &spans[spans.partition_point(|s| s.hi < partition.hi)..]
        };

        let blobs_before = reads.blobs;
        let populated = reads.fill(job, c, pbf_file, w, touched, family, partition);
        let validated = &touched[..reads.blobs - blobs_before];
        if populated.is_err() {
            if job.host.invocation.check().is_err() {
                return populated;
            }
            // The failed scan may already have populated part of its
            // blob. Replay the valid prefix so an earlier referenced
            // coordinate error still takes precedence, as in the sort
            // path. The request scan also checks the shard files before
            // returning a node failure.
            reads.fill(job, c, pbf_file, w, validated, family, partition)?;
        }
        let bad = resolve_partition(codec, &requests, w, lo, partition, rest.is_empty())?;
        if let Some(key) = bad {
            return replay_bad_coord(job, c, pbf_file, w, validated, family, key);
        }
        populated?;

        next = partition.hi;
        spans = rest;
    }
    let resolved = w.local_resolved - resolved_before;
    c.orphan_records.fetch_add(
        job.id_bucket_counts[bucket].saturating_sub(resolved),
        Relaxed,
    );
    c.node_blobs_read.fetch_add(reads.blobs as u64, Relaxed);
    c.node_straddler_blobs.fetch_add(reads.straddlers, Relaxed);
    c.pread_calls.fetch_add(reads.blobs as u64, Relaxed);
    t_resolve.record_ms(&c.table_join_ms);
    let worker_bytes = reads.buffer_peak
        + w.loader.data_buf.capacity()
        + w.slot_bufs.iter().map(Vec::capacity).sum::<usize>();
    record_max(&c.max_worker_buf_bytes, worker_bytes as u64);
    flush_bucket(job, c, w)
}

fn resolve_partition<C: ShardCodec>(
    codec: C,
    requests: &Requests<'_, '_, '_>,
    w: &mut WorkerBufs<C::Record>,
    bucket_lo: u64,
    partition: Partition,
    final_partition: bool,
) -> Result<Option<u64>> {
    let table = &w.table;
    let mut out = WalkOut {
        entry_buf: &mut w.entry_buf,
        slot_bufs: &mut w.slot_bufs,
        slot_counts: &mut w.slot_counts,
        local_resolved: &mut w.local_resolved,
    };
    let mut bad_key: Option<u64> = None;
    requests.visit(codec, &mut w.loader.data_buf, final_partition, |record| {
        let key = bucket_lo
            .checked_add(record.local())
            .ok_or_else(|| Error::internal("stage2: local node key overflows its bucket base"))?;
        let packed = table.get(partition, key);
        if packed == BAD_COORD {
            // The sort path checks referenced keys in ascending order.
            bad_key = Some(bad_key.map_or(key, |old| old.min(key)));
        } else if packed != 0 {
            let coord = crate::coord_pack::unpack((packed - 1) | PRESENT)
                .ok_or_else(|| Error::internal("stage2: populated coordinate slot is absent"))?;
            emit_resolved(requests.job, requests.counters, &record, coord, &mut out)?;
        }
        Ok(())
    })?;
    Ok(bad_key)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::unwrap_in_result)]
mod tests {
    use super::*;
    use crate::commands::altw::external::{
        IdCodec, ShardFormat, WideCodec,
        keys::{FamilyPlan, KeyPlan, NodeKeyLayout, NodeSpans},
        radix::{id_shard_path, slot_bucket_path},
        stage2::{SharedSlotBuckets, join_bucket_sorted},
    };
    use crate::host::Host;
    use crate::read::input::InputFile;
    use crate::read::wire_fixtures::{dense, field_bytes};
    use crate::scratch::RunScratch;
    use crate::test_support::assert_err_kind;
    use std::io::Write as _;
    use std::path::Path;

    type Node = (i64, i64, i64);
    type SlotRecord = [u8; super::super::RESOLVED_ENTRY_SIZE];

    fn node_blobs(
        path: &Path,
        family: Family,
        threshold: i64,
        blocks: &[&[Node]],
    ) -> (Vec<NodeBlobInfo>, NodeSpans) {
        let mut bytes = Vec::new();
        let mut blobs = Vec::new();
        let mut spans = Vec::new();
        for (blob, block) in blocks.iter().enumerate() {
            let mut group = Vec::new();
            field_bytes(&mut group, 2, &dense(block, &[]));
            let mut primitive = Vec::new();
            field_bytes(&mut primitive, 2, &group);
            let offset = bytes.len();
            field_bytes(&mut bytes, 1, &primitive); // Blob.raw
            blobs.push(NodeBlobInfo {
                data_offset: offset as u64,
                data_size: bytes.len() - offset,
                min_id: block.iter().map(|n| n.0).min().unwrap(),
                max_id: block.iter().map(|n| n.0).max().unwrap(),
                count: block.len() as u64,
            });
            let mut keys = block
                .iter()
                .filter(|node| (node.0 >= threshold) == (family == Family::Pos))
                .map(|node| family.key(node.0));
            if let Some(lo) = keys.next() {
                let mut hi = lo;
                let mut count = 1;
                for key in keys {
                    hi = key;
                    count += 1;
                }
                spans.push(KeySpan {
                    lo,
                    hi,
                    count,
                    blob: u32::try_from(blob).unwrap(),
                });
            }
        }
        std::fs::write(path, bytes).unwrap();
        let spans = match family {
            Family::Pos => NodeSpans {
                pos: spans,
                neg: Vec::new(),
            },
            Family::Neg => NodeSpans {
                pos: Vec::new(),
                neg: spans,
            },
        };
        (blobs, spans)
    }

    fn run_fixture(
        family: Family,
        blocks: &[&[Node]],
        refs: &[i64],
        budget: Option<usize>,
    ) -> Result<Vec<SlotRecord>> {
        let temp = crate::path_guard::tests::scratch();
        let input_path = temp.path().join("nodes");
        let threshold = i64::from(
            blocks
                .iter()
                .any(|block| block.iter().any(|node| node.0 < 0)),
        );
        let (blobs, spans) = node_blobs(&input_path, family, threshold, blocks);
        let input = InputFile::open(&input_path)?;
        let family_spans = spans.of(family);
        let present = FamilyPlan {
            buckets: 1,
            max_key: family_spans.last().unwrap().hi,
        };
        let absent = FamilyPlan {
            buckets: 0,
            max_key: 0,
        };
        let plan = KeyPlan {
            threshold,
            pos: if family == Family::Pos {
                present
            } else {
                absent
            },
            neg: if family == Family::Neg {
                present
            } else {
                absent
            },
            compact: present.max_key < 1 << 30,
        };
        let layout = NodeKeyLayout::new(&plan, &spans);
        let host = Host::for_tests();
        let host = host.entered();
        let scratch = RunScratch::create(temp.path(), &host)?;
        let writers = SharedSlotBuckets::create(&scratch, 2)?;
        let job = Stage2Job {
            scratch: &scratch,
            id_bucket_counts: &[refs.len() as u64],
            num_shard_workers: 2,
            layout: &layout,
            way_slot_starts: &[0],
            slot_buckets: &writers,
            slot_bucket_count: 2,
            total_slots: refs.len() as u64,
            input_pbf: &input,
            node_blob_mapping: &blobs,
            node_spans: &spans,
            inject_prepass: false,
            host: &host,
        };
        let counters = Stage2Counters::new();
        match ShardFormat::for_layout(&layout, 1)? {
            ShardFormat::Packed(codec) => test_join(codec, &job, &counters, family, refs, budget)?,
            ShardFormat::Wide(codec) => test_join(codec, &job, &counters, family, refs, budget)?,
        }
        writers.finish(&counters, &host.invocation)?;
        let mut out = Vec::new();
        for bucket in 0..2 {
            let path = slot_bucket_path(&scratch, bucket)?;
            let mut bytes = Vec::new();
            crate::scratch::open(&path)?.read_to_end(&mut bytes)?;
            let (records, rem) = bytes.as_chunks::<{ size_of::<SlotRecord>() }>();
            assert!(rem.is_empty());
            // Restore absolute positions so one record list covers both slot buckets.
            let (base, _) = super::super::slot_bucket_bounds(job.total_slots, 2, bucket);
            for record in records {
                let mut record = *record;
                let local = u32::from_le_bytes(record[..4].try_into().unwrap());
                let slot = u32::try_from(base).unwrap() + local;
                record[..4].copy_from_slice(&slot.to_le_bytes());
                out.push(record);
            }
        }
        Ok(out)
    }

    fn test_join<C: ShardCodec>(
        codec: C,
        job: &Stage2Job<'_, '_>,
        c: &Stage2Counters,
        family: Family,
        refs: &[i64],
        budget: Option<usize>,
    ) -> Result<()> {
        let (_, bucket_lo, _) = job.layout.bucket(0).unwrap();
        let mut slot = 0;
        for (worker, refs) in refs.chunks(refs.len().div_ceil(2)).enumerate() {
            let path = id_shard_path(job.scratch, worker, 0)?;
            let file = crate::scratch::create(&path)?;
            let mut file = std::io::BufWriter::new(file);
            for &id in refs {
                let mut record = [0; 2 * ID_WORD_SIZE];
                let len = codec.encode(
                    family.key(id) - bucket_lo,
                    slot % 11 == 0,
                    0,
                    slot,
                    &mut record,
                )?;
                file.write_all(&record[..len])?;
                slot += 1;
            }
            file.flush()?;
        }

        let file = job.input_pbf.file();
        let mut w = WorkerBufs::new(job.slot_bucket_count);
        if let Some(budget) = budget {
            join_bucket(codec, job, c, file, 0, &mut w, budget)?;
            assert!(w.table.bytes() <= budget);
            assert_eq!(w.loader.records.capacity(), 0);
            assert_eq!(w.node_tuples.capacity(), 0);
            assert!(w.loader.data_buf.capacity() <= ALTW_STAGE2_SHARD_READ_BYTES + ID_WORD_SIZE);
        } else {
            join_bucket_sorted(codec, job, c, file, 0, &mut w)?;
        }
        assert_eq!(
            job.id_bucket_counts[0] - w.local_resolved,
            c.orphan_records.load(Relaxed),
        );
        Ok(())
    }

    fn assert_matches_sorted(
        family: Family,
        blocks: &[&[Node]],
        refs: &[i64],
        budget: usize,
        missing: usize,
    ) -> Vec<SlotRecord> {
        let mut sorted = run_fixture(family, blocks, refs, None).unwrap();
        let streamed = run_fixture(family, blocks, refs, Some(budget)).unwrap();
        let mut canonical = streamed.clone();
        sorted.sort_unstable();
        canonical.sort_unstable();
        assert_eq!(canonical, sorted);
        assert_eq!(canonical.len(), refs.len() - missing);
        streamed
    }

    #[test]
    fn dense_partition_stream_matches_sort_with_skew_and_boundary_duplicates() {
        let left = [(0, 0, 0), (1, 9, 8), (3, 7, -6), (3, 7, -6)];
        let right = [(3, 7, -6), (4, 5, -4), (6, -3, 2), (7, -1, 1)];
        let mut refs = vec![7, 4, 3, 0, 2, 1, 6, 5, 3];
        refs.extend(std::iter::repeat_n(3, 50_000));
        // Four dense slots per partition. The duplicate at the end of
        // the first partition continues into a blob spanning both.
        assert_matches_sorted(Family::Pos, &[&left, &right], &refs, 32, 2);
        let streamed = assert_matches_sorted(Family::Pos, &[&left, &right], &refs, 64, 2);
        let positions: Vec<_> = streamed
            .iter()
            .map(|r| u32::from_le_bytes(r[..4].try_into().unwrap()))
            .collect();
        let original: Vec<_> = (0..u32::try_from(refs.len()).unwrap())
            .filter(|&i| i != 4 && i != 7)
            .collect();
        assert_eq!(positions, original);
    }

    #[test]
    fn sparse_rank_partitions_match_sort_for_wide_negative_ids() {
        let far = -(1i64 << 40);
        let first = [(0, -900_000_000, 1_800_000_000), (-1, 0, 0)];
        let middle = [(far, 900_000_000, -1_800_000_000), (far - 1, -4, 5)];
        let last = [(far - 1, -4, 5), (i64::MIN, 6, -7)];
        let refs = [i64::MIN, far + 1, -1, far - 1, far - 2, 0, far, far - 1];
        assert_matches_sorted(Family::Neg, &[&first, &middle, &last], &refs, 64, 2);
        assert_matches_sorted(Family::Neg, &[&first, &middle, &last], &refs, 128, 2);
    }

    #[test]
    fn dense_partitions_join_both_families_of_a_blob_crossing_zero() {
        let mixed = [
            (0, 0, 0),
            (-1, 11, 12),
            (-3, 31, 32),
            (1, -11, -12),
            (3, -31, -32),
        ];
        assert_matches_sorted(Family::Neg, &[&mixed], &[-3, -2, 0, -1], 16, 1);
        assert_matches_sorted(Family::Pos, &[&mixed], &[3, 2, 1], 16, 1);
    }

    #[test]
    fn duplicate_refusals_match_sort_and_ignore_unreferenced_bad_nodes() {
        let first = [(1, 1, 2), (2, 3, 4), (3, 900_000_001, 0)];
        // The first duplicate conflicts, even though a later one is
        // outside the domain. A referenced invalid first node is a
        // domain error, regardless of the coordinates of its duplicate.
        let second = [(3, 5, 6), (4, 7, 8)];
        let third = [(4, 9, 10), (4, 11, 1_800_000_001), (5, 1, 2)];
        let blobs = [&first[..], &second[..], &third[..]];
        assert_matches_sorted(Family::Pos, &blobs, &[2, 1, 5, 0], 64, 1);
        for refs in [&[4, 2][..], &[4, 3][..]] {
            let sorted = run_fixture(Family::Pos, &blobs, refs, None).unwrap_err();
            let streamed = run_fixture(Family::Pos, &blobs, refs, Some(32)).unwrap_err();
            assert_eq!(streamed.to_string(), sorted.to_string());
        }
    }

    #[test]
    fn earlier_coordinate_refusal_precedes_a_later_blob_claim_failure() {
        let first = [(1, 900_000_001, 0)];
        let unsorted = [(6, 0, 0), (8, 0, 0), (7, 0, 0), (9, 0, 0)];
        let blobs = [&first[..], &unsorted[..]];
        let sorted = run_fixture(Family::Pos, &blobs, &[1], None).unwrap_err();
        let streamed = run_fixture(Family::Pos, &blobs, &[1], Some(128)).unwrap_err();
        assert_eq!(streamed.to_string(), sorted.to_string());
    }

    #[test]
    fn shard_visitors_preserve_order_across_partial_records() {
        let packed = IdCodec::new((1 << 31) - 1, 1 << 31).unwrap();
        assert_eq!(packed.slot_bits(), 1);
        assert_split_record(packed, "inside an escaped record");
        assert_split_record(WideCodec, "is not a multiple of");
    }

    fn assert_split_record<C: ShardCodec>(codec: C, expected_message: &str) {
        let mut first = [0u8; 2 * ID_WORD_SIZE];
        let mut second = [0u8; 2 * ID_WORD_SIZE];
        let first_len = codec.encode(23, false, 0, 7, &mut first).unwrap();
        let second_len = codec.encode(11, true, 1, 15, &mut second).unwrap();
        assert_eq!(second_len, 2 * ID_WORD_SIZE);
        let bytes = [&first[..first_len], &second[..second_len]].concat();
        let cut = first_len + ID_WORD_SIZE;
        let mut out = Vec::new();
        let used = codec
            .visit_records(&bytes[..cut], false, |record| {
                out.push(record);
                Ok(())
            })
            .unwrap();
        assert_eq!(used, first_len);
        assert_err_kind!(
            codec.decode_into(&bytes[..cut], &mut Vec::new()),
            crate::ErrorKind::Internal { msg, .. } if msg.contains(expected_message),
        );
        codec.decode_into(&bytes[used..], &mut out).unwrap();
        assert_eq!(
            out.iter().map(ShardRecord::local).collect::<Vec<_>>(),
            [23, 11]
        );
    }
}
