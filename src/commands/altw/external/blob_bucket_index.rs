/// Classification of how a way blob intersects a slot bucket.
///
/// A blob's slot range is assumed to be smaller than a bucket's slot
/// range (enforced by the pipeline's startup assertion). So a blob
/// either fits fully within one bucket or straddles exactly two
/// adjacent buckets (contributing a left half to the earlier bucket
/// and a right half to the later bucket).
#[derive(Debug, PartialEq, Eq)]
pub(super) enum BlobBucketIntersection {
    /// Blob's slot range is entirely within the bucket.
    FullyContained { blob_idx: usize },
    /// Blob extends before the bucket start; this bucket contains the
    /// right-hand slot range `[bucket_start_slot, blob_end_slot)`.
    RightHalf { blob_idx: usize },
    /// Blob extends past the bucket end; this bucket contains the
    /// left-hand slot range `[blob_start_slot, bucket_end_slot)`.
    LeftHalf { blob_idx: usize },
}

/// Classify all way blobs intersecting slot range
/// [bucket_start_slot, bucket_end_slot). Returns intersections in
/// blob-index order.
///
/// `way_slot_starts[i]` is the starting slot of blob `i`; blob `i`
/// spans [way_slot_starts[i], way_slot_starts[i+1]) for i < N-1 and
/// [way_slot_starts[N-1], total_slots) for the last.
///
/// Empty blobs (slot range of length 0) are omitted.
///
/// Returns `Err` if any intersecting blob's slot range is wider than
/// the bucket's (structural assumption violated - indicates a bug upstream).
pub(super) fn classify_blobs_in_bucket(
    bucket_start_slot: u64,
    bucket_end_slot: u64,
    way_slot_starts: &[u64],
    total_slots: u64,
) -> std::result::Result<Vec<BlobBucketIntersection>, String> {
    let bucket_size = bucket_end_slot - bucket_start_slot;
    let n = way_slot_starts.len();

    // partition_point finds the first blob strictly after bucket_start; the
    // blob immediately before that may still have its tail inside the bucket.
    let j = way_slot_starts.partition_point(|x| *x <= bucket_start_slot);
    let first_i = if j > 0 { j - 1 } else { 0 };

    let mut result = Vec::new();

    for i in first_i..n {
        let blob_start = way_slot_starts[i];
        let blob_end = if i + 1 < n {
            way_slot_starts[i + 1]
        } else {
            total_slots
        };

        if blob_start >= bucket_end_slot {
            break;
        }
        if blob_end == blob_start {
            continue;
        }

        let blob_size = blob_end - blob_start;
        if blob_size > bucket_size {
            return Err(format!(
                "blob {i} slot range [{blob_start}, {blob_end}) is wider ({blob_size}) than \
                 bucket [{bucket_start_slot}, {bucket_end_slot}) ({bucket_size}); \
                 structural assumption violated"
            ));
        }

        if blob_end <= bucket_start_slot {
            continue;
        }

        let intersection = if blob_start >= bucket_start_slot && blob_end <= bucket_end_slot {
            BlobBucketIntersection::FullyContained { blob_idx: i }
        } else if blob_start < bucket_start_slot {
            BlobBucketIntersection::RightHalf { blob_idx: i }
        } else {
            BlobBucketIntersection::LeftHalf { blob_idx: i }
        };

        result.push(intersection);
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_empty_inputs() {
        let result = classify_blobs_in_bucket(0, 100, &[], 0).expect("classify");
        assert!(result.is_empty());
    }

    #[test]
    fn classify_single_blob_fully_contained() {
        // Blob 0: [20, 80), bucket: [0, 100)
        let result = classify_blobs_in_bucket(0, 100, &[20], 80).expect("classify");
        assert_eq!(result, vec![BlobBucketIntersection::FullyContained { blob_idx: 0 }]);
    }

    #[test]
    fn classify_single_blob_left_half() {
        // Blob 0: [50, 150), bucket: [0, 100)
        // Blob extends past bucket end → LeftHalf
        let result = classify_blobs_in_bucket(0, 100, &[50], 150).expect("classify");
        assert_eq!(result, vec![BlobBucketIntersection::LeftHalf { blob_idx: 0 }]);
    }

    #[test]
    fn classify_single_blob_right_half() {
        // Blob 0: [0, 50), bucket: [25, 100)
        // Blob starts before bucket_start → RightHalf
        let result = classify_blobs_in_bucket(25, 100, &[0], 50).expect("classify");
        assert_eq!(result, vec![BlobBucketIntersection::RightHalf { blob_idx: 0 }]);
    }

    #[test]
    fn classify_multiple_blobs_in_bucket() {
        // 5 blobs with a bucket of [100, 600):
        //   blob 0: [50, 150)   → RightHalf (started in prior bucket)
        //   blob 1: [150, 250)  → FullyContained
        //   blob 2: [250, 350)  → FullyContained
        //   blob 3: [350, 450)  → FullyContained
        //   blob 4: [450, 650)  → LeftHalf (extends past bucket end)
        let starts = [50, 150, 250, 350, 450];
        let total_slots = 650;
        let result = classify_blobs_in_bucket(100, 600, &starts, total_slots).expect("classify");
        assert_eq!(result.len(), 5);
        assert_eq!(result[0], BlobBucketIntersection::RightHalf { blob_idx: 0 });
        assert_eq!(result[1], BlobBucketIntersection::FullyContained { blob_idx: 1 });
        assert_eq!(result[2], BlobBucketIntersection::FullyContained { blob_idx: 2 });
        assert_eq!(result[3], BlobBucketIntersection::FullyContained { blob_idx: 3 });
        assert_eq!(result[4], BlobBucketIntersection::LeftHalf { blob_idx: 4 });
    }

    #[test]
    fn classify_boundary_exact_match() {
        // Blob end exactly equals bucket end → FullyContained, not LeftHalf.
        // Blob 0: [50, 100), bucket: [0, 100)
        let result = classify_blobs_in_bucket(0, 100, &[50], 100).expect("classify");
        assert_eq!(result, vec![BlobBucketIntersection::FullyContained { blob_idx: 0 }]);

        // Blob start exactly equals bucket start → FullyContained (if blob fits within bucket).
        // Blob 0: [0, 80), bucket: [0, 100)
        let result = classify_blobs_in_bucket(0, 100, &[0], 80).expect("classify");
        assert_eq!(result, vec![BlobBucketIntersection::FullyContained { blob_idx: 0 }]);
    }

    #[test]
    fn classify_empty_blob_omitted() {
        // way_slot_starts = [10, 10, 20], total_slots = 20
        // blob 0: [10, 10) → empty, skip
        // blob 1: [10, 20) → FullyContained in bucket [0, 30)
        // blob 2: [20, 20) → empty (total_slots == way_slot_starts[2]), skip
        let starts = [10, 10, 20];
        let result = classify_blobs_in_bucket(0, 30, &starts, 20).expect("classify");
        assert_eq!(result, vec![BlobBucketIntersection::FullyContained { blob_idx: 1 }]);
    }

    #[test]
    fn classify_last_blob_uses_total_slots() {
        // 2 blobs: starts = [0, 50], total_slots = 80
        // blob 0: [0, 50)
        // blob 1: [50, 80)  ← end comes from total_slots, not starts[2]
        // bucket [0, 100): both FullyContained
        let starts = [0, 50];
        let result = classify_blobs_in_bucket(0, 100, &starts, 80).expect("classify");
        assert_eq!(result, vec![
            BlobBucketIntersection::FullyContained { blob_idx: 0 },
            BlobBucketIntersection::FullyContained { blob_idx: 1 },
        ]);
    }

    #[test]
    fn classify_blob_wider_than_bucket_errors() {
        // Blob 0: [0, 100), bucket: [0, 10) - blob is 10x wider than bucket
        let result = classify_blobs_in_bucket(0, 10, &[0], 100);
        assert!(result.is_err(), "expected Err for blob wider than bucket");
    }
}
