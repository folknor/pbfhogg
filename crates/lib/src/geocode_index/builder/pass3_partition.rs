//! Parallel first partition with the same permutation as the cyclic Lomuto
//! partition in `pass3_sort`. The less-than side uses a prefix sum of chunk
//! counts. The other side gathers through the original swaps in reverse.

use std::mem::MaybeUninit;

use rayon::prelude::*;

use crate::geocode_index::format::BucketRecord;
use crate::host::ExecutorRef;

pub(super) fn partition(
    executor: ExecutorRef<'_>,
    entries: &mut Vec<BucketRecord>,
    pivot: usize,
    chunk_records: usize,
) -> usize {
    entries.swap(0, pivot);
    let (pivot, saved) = (entries[0], entries[1]);
    let key = pivot.cell_id;
    // The legacy partition scans this slice and processes `saved` last.
    let body = &entries[2..];
    let counts: Vec<_> = executor.install(|| {
        body.par_chunks(chunk_records)
            .map(|chunk| chunk.iter().filter(|entry| entry.cell_id < key).count())
            .collect()
    });
    let body_left: usize = counts.iter().sum();
    let saved_left = saved.cell_id < key;
    let mid = body_left + usize::from(saved_left);
    if mid == 0 {
        // With no smaller record, the cyclic scan restores the unchanged rest.
        return 0;
    }

    let len = entries.len();
    let mut out: Vec<BucketRecord> = Vec::with_capacity(len);
    let (head, right) = out.spare_capacity_mut()[..len].split_at_mut(mid + 1);
    let (pivot_slot, left) = head.split_at_mut(1);
    pivot_slot[0].write(pivot);
    let (body_out, saved_out) = left.split_at_mut(body_left);
    if saved_left {
        saved_out[0].write(saved);
    }
    let mut links = vec![0; body.len()];
    fill_left_and_links(
        executor,
        body,
        key,
        &counts,
        body_out,
        &mut links,
        chunk_records,
    );

    // If `saved` compares >=, the final step writes it at `mid` in the rest
    // of the bucket. Every other position in the right side was finalized
    // when the scan passed it and follows the links from the preceding step.
    let (saved_out, right) = right.split_at_mut(usize::from(!saved_left));
    if !saved_left {
        saved_out[0].write(saved);
    }
    let start = mid + usize::from(!saved_left);
    executor.install(|| {
        right
            .par_chunks_mut(chunk_records)
            .enumerate()
            .for_each(|(i, chunk)| {
                let first = start + i * chunk_records;
                for (j, dst) in chunk.iter_mut().enumerate() {
                    dst.write(right_record(body, &links, first + j));
                }
            });
    });

    // SAFETY: the pivot, both sides and the saved record's slot cover all
    // `len` elements. The checked chunk lengths and the two joined passes
    // ensure each has been initialized. No length is set if a worker panics.
    unsafe { out.set_len(len) };
    out.swap(0, mid);
    *entries = out;
    mid
}

fn fill_left_and_links(
    executor: ExecutorRef<'_>,
    body: &[BucketRecord],
    key: u64,
    counts: &[usize],
    mut left: &mut [MaybeUninit<BucketRecord>],
    links: &mut [usize],
    chunk_records: usize,
) {
    let mut prefix = 0;
    let chunks: Vec<_> = body
        .chunks(chunk_records)
        .zip(links.chunks_mut(chunk_records))
        .zip(counts)
        .map(|((records, links), &count)| {
            let (out, remaining) = std::mem::take(&mut left).split_at_mut(count);
            left = remaining;
            let prior = prefix;
            prefix += count;
            (records, links, out, prior)
        })
        .collect();
    assert!(left.is_empty());

    executor.install(|| {
        chunks
            .into_par_iter()
            .enumerate()
            .for_each(|(i, (records, links, out, prior))| {
                let start = i * chunk_records;
                let mut written = 0;
                for (offset, &record) in records.iter().enumerate() {
                    let pos = start + offset;
                    if record.cell_id >= key {
                        // A code above the link indices refers directly to input.
                        links[offset] = body.len() + pos;
                        continue;
                    }
                    out[written].write(record);
                    written += 1;
                    let prior_link = prior + written - 1;
                    // Collapse links within the chunk as we go. This also handles
                    // long runs below the pivot without retracing them at gather.
                    links[offset] = if (start..pos).contains(&prior_link) {
                        links[prior_link - start]
                    } else {
                        prior_link
                    };
                }
                assert_eq!(written, out.len());
            });
    });
}

/// The cyclic scan is equivalent to always swapping scan slot `j` with
/// `rank(j)`, the number of smaller records before it in the rotated input.
/// A displaced record at `j` is either the preceding input (if it compared
/// greater or equal), or the prior contents of `rank(j)`, whose own displaced record came
/// from an earlier step. `links[j - 1]` encodes that predecessor.
fn right_record(body: &[BucketRecord], links: &[usize], j: usize) -> BucketRecord {
    let mut source = links[j - 1];
    while source < body.len() {
        let next = links[source];
        // A self-link occurs only in the initial all-smaller run, where
        // there was no displaced record. The right side cannot reach it.
        assert_ne!(source, next);
        source = next;
    }
    body[source - body.len()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geocode_index::format::BUCKET_ENTRY_ADDR;
    use crate::host::Executor;

    #[test]
    fn prefix_partition_matches_cyclic_partition_for_every_small_split() {
        let executor = Executor::for_tests();
        // With the pivot key fixed, only the less-than decision and the
        // pivot's original position affect the permutation. Distinct payloads
        // expose every misplaced record, including equal keys.
        for mask in 0..(1 << 6) {
            for pivot in 0..6 {
                let input: Vec<_> = (0..6u32)
                    .map(|index| BucketRecord {
                        cell_id: u64::from(index == pivot || (mask & (1 << index)) == 0),
                        entry_type: BUCKET_ENTRY_ADDR,
                        index,
                        segment: 0,
                    })
                    .collect();
                let pivot = usize::try_from(pivot).expect("pivot index");
                let mut expected = input.clone();
                let mid = super::super::partition::<false>(&mut expected, pivot);
                for chunk_records in [1, 2, 4] {
                    let mut actual = input.clone();
                    assert_eq!(
                        partition(executor.borrowed(), &mut actual, pivot, chunk_records),
                        mid
                    );
                    assert_eq!(
                        actual, expected,
                        "mask {mask}, pivot {pivot}, chunk {chunk_records}"
                    );
                }
            }
        }
    }

    #[test]
    fn prefix_partition_preserves_a_record_displaced_across_many_chunks() {
        let executor = Executor::for_tests();
        let mut actual: Vec<_> = (0..257)
            .map(|index| BucketRecord {
                // The pivot and the first scanned record tie. Every later
                // step displaces that scanned record one slot further right.
                cell_id: u64::from(matches!(index, 0 | 2)),
                entry_type: BUCKET_ENTRY_ADDR,
                index,
                segment: 0,
            })
            .collect();
        let mut expected = actual.clone();
        let mid = super::super::partition::<false>(&mut expected, 0);
        assert_eq!(partition(executor.borrowed(), &mut actual, 0, 8), mid);
        assert_eq!(actual.last().map(|entry| entry.index), Some(2));
        assert_eq!(actual, expected);
    }
}
