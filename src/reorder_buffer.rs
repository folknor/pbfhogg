use std::collections::VecDeque;

/// Sequence-number reorder buffer for out-of-order producer results.
///
/// Accepts items tagged with monotonic sequence numbers and yields only the
/// next contiguous ready item from the front.
pub(crate) struct ReorderBuffer<T> {
    next_seq: usize,
    pending: VecDeque<Option<T>>,
}

impl<T> ReorderBuffer<T> {
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self {
            next_seq: 0,
            pending: VecDeque::with_capacity(capacity),
        }
    }

    /// Insert an item at sequence number `seq`.
    pub(crate) fn push(&mut self, seq: usize, item: T) {
        assert!(
            seq >= self.next_seq,
            "reorder buffer received stale sequence number: {seq} < {}",
            self.next_seq
        );
        let slot_idx = seq - self.next_seq;
        if slot_idx >= self.pending.len() {
            self.pending.resize_with(slot_idx + 1, || None);
        }
        assert!(self.pending[slot_idx].is_none(), "duplicate sequence number: {seq}");
        self.pending[slot_idx] = Some(item);
    }

    /// Pop the next contiguous ready item, if available.
    pub(crate) fn pop_ready(&mut self) -> Option<T> {
        if !self.pending.front().is_some_and(Option::is_some) {
            return None;
        }
        let item = self.pending.pop_front().and_then(|x| x);
        if item.is_some() {
            self.next_seq += 1;
        }
        item
    }
}
