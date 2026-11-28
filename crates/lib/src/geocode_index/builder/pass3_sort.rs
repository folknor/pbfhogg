//! The Stage B bucket sort must reproduce the old `sort_unstable_by_key`
//! permutation. Equal cells can carry different payloads, and the encoder
//! preserves their order within each list. Stable sorting would instead
//! preserve scratch order, which the old sort can change.
//!
//! This specializes the ipnsort in the baseline Rust nightly (2026-11-19)
//! (by Lukas Bergdoll and Orson Peters) to the 16-byte `BucketRecord` and
//! its cell key. Pivot selection, cyclic partitioning, the predecessor-key
//! test and the depth limit determine the permutation. Once partitioned,
//! the two children are disjoint and can execute in any order. Keep the
//! tests against the old standard-library call when updating the toolchain.

use crate::geocode_index::format::BucketRecord;
use crate::host::ExecutorRef;
use crate::tunables::{
    GEOCODE_STAGE_B_PARTITION_CHUNK_RECORDS, GEOCODE_STAGE_B_SORT_PAR_MIN_RECORDS,
};

#[path = "pass3_partition.rs"]
mod first_partition;

// Algorithm constants from ipnsort, not scheduling tunables. The latter
// threshold is for the general small sort selected for a 16-byte record.
const RECURSIVE_PIVOT_MIN: usize = 64;
const SMALL_SORT_MAX: usize = 32;

pub(super) fn sort_bucket(executor: ExecutorRef<'_>, entries: &mut Vec<BucketRecord>) {
    if entries.len() <= 2 * GEOCODE_STAGE_B_SORT_PAR_MIN_RECORDS
        || executor.install(rayon::current_num_threads) == 1
    {
        entries.sort_unstable_by_key(|entry| entry.cell_id);
        return;
    }

    // ipnsort checks for an entire monotone run only at the root. A
    // descending run must be strict so reversal never reverses ties.
    if entries[0].cell_id <= entries[1].cell_id {
        if entries.is_sorted_by_key(|entry| entry.cell_id) {
            return;
        }
    } else if entries.windows(2).all(|p| p[0].cell_id > p[1].cell_id) {
        entries.reverse();
        return;
    }

    let limit = 2 * entries.len().ilog2() - 1;
    let pivot = choose_pivot(entries);
    let mid = if entries.len() > GEOCODE_STAGE_B_PARTITION_CHUNK_RECORDS {
        first_partition::partition(
            executor,
            entries,
            pivot,
            GEOCODE_STAGE_B_PARTITION_CHUNK_RECORDS,
        )
    } else {
        partition::<false>(entries, pivot)
    };
    // The scope owns all spawned branches and waits for them before encoding.
    executor.scope(move |scope| {
        let (right, pivot) = sort_left_partition(scope, entries, mid, None, limit);
        quicksort(scope, right, Some(pivot), limit);
    });
}

fn quicksort<'scope>(
    scope: &rayon::Scope<'scope>,
    mut entries: &'scope mut [BucketRecord],
    mut predecessor: Option<u64>,
    mut limit: u32,
) {
    while entries.len() > SMALL_SORT_MAX {
        if limit == 0 {
            heapsort(entries);
            return;
        }
        limit -= 1;
        let pivot = choose_pivot(entries);
        if predecessor == Some(entries[pivot].cell_id) {
            // This is the smallest key left. The legacy <= partition also
            // fixes the order of this run of equal keys.
            let mid = partition::<true>(entries, pivot);
            entries = &mut entries[mid + 1..];
            predecessor = None;
            continue;
        }

        let mid = partition::<false>(entries, pivot);
        let (right, pivot) = sort_left_partition(scope, entries, mid, predecessor, limit);
        entries = right;
        predecessor = Some(pivot);
    }

    // The standard library uses the stable general small sort for this
    // record type in a quicksort leaf. Its standalone call may use stable
    // insertion or a monotone shortcut at this size, with the same result.
    entries.sort_unstable_by_key(|entry| entry.cell_id);
}

fn sort_left_partition<'scope>(
    scope: &rayon::Scope<'scope>,
    entries: &'scope mut [BucketRecord],
    mid: usize,
    predecessor: Option<u64>,
    limit: u32,
) -> (&'scope mut [BucketRecord], u64) {
    let (left, rest) = entries.split_at_mut(mid);
    let (pivot, right) = rest.split_at_mut(1);
    if left.len().min(right.len()) >= GEOCODE_STAGE_B_SORT_PAR_MIN_RECORDS {
        scope.spawn(move |scope| quicksort(scope, left, predecessor, limit));
    } else {
        quicksort(scope, left, predecessor, limit);
    }
    (right, pivot[0].cell_id)
}

fn choose_pivot(entries: &[BucketRecord]) -> usize {
    let stride = entries.len() / 8;
    let median = |start| {
        if entries.len() >= RECURSIVE_PIVOT_MIN {
            choose_pivot(&entries[start..start + stride]) + start
        } else {
            start
        }
    };
    let (a, b, c) = (median(0), median(4 * stride), median(7 * stride));
    let ab = entries[a].cell_id < entries[b].cell_id;
    let ac = entries[a].cell_id < entries[c].cell_id;
    if ab != ac {
        a
    } else if (entries[b].cell_id < entries[c].cell_id) ^ ab {
        c
    } else {
        b
    }
}

fn partition<const INCLUDE_EQUAL: bool>(entries: &mut [BucketRecord], pivot: usize) -> usize {
    entries.swap(0, pivot);
    let (head, rest) = entries.split_at_mut(1);
    let key = head[0].cell_id;
    let belongs_left = |cell_id| {
        if INCLUDE_EQUAL {
            cell_id <= key
        } else {
            cell_id < key
        }
    };

    // Lomuto cyclic partition. Hold the first record out of the scan and
    // fill the preceding slot at each step. Visiting the held record last
    // is part of the legacy tie permutation.
    let saved = rest[0];
    let mut left = 0;
    for right in 1..rest.len() {
        let record = rest[right];
        rest[right - 1] = rest[left];
        rest[left] = record;
        left += usize::from(belongs_left(record.cell_id));
    }
    let last = rest.len() - 1;
    rest[last] = rest[left];
    rest[left] = saved;
    left += usize::from(belongs_left(saved.cell_id));
    entries.swap(0, left);
    left
}

fn heapsort(entries: &mut [BucketRecord]) {
    // Build the same max heap and prefer the left child on equal keys.
    let len = entries.len();
    for node in (0..len / 2).rev() {
        sift_down(entries, node);
    }
    for end in (1..len).rev() {
        entries.swap(0, end);
        sift_down(&mut entries[..end], 0);
    }
}

fn sift_down(entries: &mut [BucketRecord], mut node: usize) {
    while node < entries.len() / 2 {
        let mut child = 2 * node + 1;
        if child + 1 < entries.len() && entries[child].cell_id < entries[child + 1].cell_id {
            child += 1;
        }
        if entries[node].cell_id >= entries[child].cell_id {
            break;
        }
        entries.swap(node, child);
        node = child;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geocode_index::format::BUCKET_ENTRY_ADDR;
    use crate::host::Executor;

    #[test]
    fn parallel_bucket_sort_matches_legacy_permutation() {
        let executor = Executor::for_tests();
        // Large enough for parallel partition branches, with payloads that
        // make every equal-key permutation observable.
        let len = (4 * GEOCODE_STAGE_B_SORT_PAR_MIN_RECORDS)
            .max(GEOCODE_STAGE_B_PARTITION_CHUNK_RECORDS)
            + 17;
        let n = u32::try_from(len).expect("fixture length");
        let mut random = 0x9e37_79b9_u32;
        for (case, keys) in [
            ("nondecreasing", (0..n).map(|i| i / 5).collect::<Vec<_>>()),
            ("descending", (0..n).rev().collect()),
            ("alternating", (0..n).map(|i| i % 2).collect()),
            (
                "dominant cell",
                (0..n).map(|i| u32::from(i.is_multiple_of(127))).collect(),
            ),
            (
                "random ties",
                (0..n)
                    .map(|_| {
                        random = random.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                        (random >> 12) & 0xfff
                    })
                    .collect(),
            ),
        ] {
            let mut actual: Vec<_> = (0..n)
                .zip(keys)
                .map(|(index, key)| BucketRecord {
                    cell_id: u64::from(key),
                    entry_type: BUCKET_ENTRY_ADDR,
                    index,
                    segment: 0,
                })
                .collect();
            let mut expected = actual.clone();
            expected.sort_unstable_by_key(|entry| entry.cell_id);
            if case == "random ties" {
                let mut stable = actual.clone();
                stable.sort_by_key(|entry| entry.cell_id);
                assert!(
                    expected != stable,
                    "fixture must expose the legacy tie order"
                );
            }
            sort_bucket(executor.borrowed(), &mut actual);
            assert!(actual == expected, "bucket permutation differs for {case}");
        }
    }
}
