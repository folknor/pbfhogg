//! The retention rule for worker scratch: a buffer a worker reuses across
//! tasks keeps its capacity up to `MAX_RETAINED_SCRATCH_CAPACITY` and is
//! released past it, so one outlier task does not pin its peak for the rest
//! of the run. `BlockBuilder` applies the same line to its own buffers.
//!
//! It lives below both the read engines and the commands because both
//! reuse scratch across blocks: the readers' string-table and group offset
//! lists, and the commands' per-task lists.

/// Clear a worker scratch vector for the next task, releasing it if one
/// task grew it past the retention limit, so it does not pin an outlier for
/// the run.
pub(crate) fn release_scratch<T>(v: &mut Vec<T>) {
    v.clear();
    if v.capacity().saturating_mul(std::mem::size_of::<T>())
        > crate::tunables::MAX_RETAINED_SCRATCH_CAPACITY
    {
        drop(std::mem::take(v));
    }
}

#[cfg(test)]
mod tests {
    use super::release_scratch;
    use crate::tunables::MAX_RETAINED_SCRATCH_CAPACITY;

    #[test]
    fn keeps_capacity_at_the_line_and_releases_past_it() {
        let pair = size_of::<(u32, u32)>();
        let mut kept: Vec<(u32, u32)> = Vec::with_capacity(MAX_RETAINED_SCRATCH_CAPACITY / pair);
        kept.push((1, 2));
        let cap = kept.capacity();
        release_scratch(&mut kept);
        assert!(kept.is_empty());
        assert_eq!(kept.capacity(), cap);

        let mut released: Vec<(u32, u32)> =
            Vec::with_capacity(MAX_RETAINED_SCRATCH_CAPACITY / pair + 1);
        released.push((1, 2));
        release_scratch(&mut released);
        assert!(released.is_empty());
        assert_eq!(released.capacity(), 0);
    }
}
