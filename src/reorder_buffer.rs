use std::collections::VecDeque;

use crate::tunables::DEFAULT_MAX_WINDOW;

/// Sequence-number reorder buffer for out-of-order producer results.
///
/// Accepts items tagged with monotonic sequence numbers and yields only the
/// next contiguous ready item from the front.
pub(crate) struct ReorderBuffer<T> {
    next_seq: usize,
    filled: usize,
    max_window: usize,
    pending: VecDeque<Option<T>>,
}

impl<T> ReorderBuffer<T> {
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self::with_max_window(capacity, DEFAULT_MAX_WINDOW.max(capacity))
    }

    /// As [`ReorderBuffer::with_capacity`], but with an explicit ceiling on
    /// how far ahead of the next expected sequence number a pushed item may
    /// sit. `max_window` must be at least `capacity`.
    pub(crate) fn with_max_window(capacity: usize, max_window: usize) -> Self {
        assert!(
            max_window >= capacity,
            "reorder buffer max_window ({max_window}) below its capacity ({capacity})"
        );
        Self {
            next_seq: 0,
            filled: 0,
            max_window,
            pending: VecDeque::with_capacity(capacity),
        }
    }

    /// Insert an item at sequence number `seq`.
    ///
    /// The stale-seq and duplicate-seq asserts panic deliberately:
    /// seqs originate from `enumerate()` in the producer, so hitting
    /// either condition is an upstream programming error (retry loop
    /// re-sending a seq, wrong seq source, etc.), not a recoverable
    /// runtime condition. A panic kills the writer thread and surfaces
    /// via `join().map_err(...)?` so the caller sees a hard failure
    /// rather than silent drift.
    pub(crate) fn push(&mut self, seq: usize, item: T) {
        assert!(
            seq >= self.next_seq,
            "reorder buffer received stale sequence number: {seq} < {}",
            self.next_seq
        );
        let slot_idx = seq - self.next_seq;
        assert!(
            slot_idx < self.max_window,
            "reorder buffer window exceeded: seq {seq} is {slot_idx} ahead of \
             {} (max {})",
            self.next_seq,
            self.max_window
        );
        if slot_idx >= self.pending.len() {
            self.pending.resize_with(slot_idx + 1, || None);
        }
        assert!(
            self.pending[slot_idx].is_none(),
            "duplicate sequence number: {seq}"
        );
        self.pending[slot_idx] = Some(item);
        self.filled += 1;
    }

    /// Pop the next contiguous ready item, if available.
    pub(crate) fn pop_ready(&mut self) -> Option<T> {
        if !self.pending.front().is_some_and(Option::is_some) {
            return None;
        }
        let item = self.pending.pop_front().and_then(|x| x);
        if item.is_some() {
            self.filled -= 1;
            self.next_seq += 1;
        }
        item
    }

    /// The sequence number the buffer will accept next, which is also the
    /// count of items already popped in order.
    ///
    /// End-of-stream completeness checks compare this against the producer's
    /// announced total: `pending_len() == 0` only proves no item is *blocked*
    /// behind a gap, and a gap at the tail leaves nothing blocked at all.
    pub(crate) fn next_seq(&self) -> usize {
        self.next_seq
    }

    pub(crate) fn filled_len(&self) -> usize {
        self.filled
    }

    pub(crate) fn pending_len(&self) -> usize {
        self.pending.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_order_push() {
        let mut rb = ReorderBuffer::with_capacity(4);
        rb.push(0, "a");
        rb.push(1, "b");
        rb.push(2, "c");
        assert_eq!(rb.pop_ready(), Some("a"));
        assert_eq!(rb.pop_ready(), Some("b"));
        assert_eq!(rb.pop_ready(), Some("c"));
        assert_eq!(rb.pop_ready(), None);
    }

    #[test]
    fn out_of_order_push() {
        let mut rb = ReorderBuffer::with_capacity(4);

        // Push 1 first - can't pop yet (0 missing)
        rb.push(1, "b");
        assert_eq!(rb.pop_ready(), None);

        // Push 0 - now 0 and 1 are contiguous
        rb.push(0, "a");
        assert_eq!(rb.pop_ready(), Some("a"));
        assert_eq!(rb.pop_ready(), Some("b"));

        // Push 2 - immediately ready
        rb.push(2, "c");
        assert_eq!(rb.pop_ready(), Some("c"));
        assert_eq!(rb.pop_ready(), None);
    }

    #[test]
    fn empty_buffer() {
        let mut rb: ReorderBuffer<i32> = ReorderBuffer::with_capacity(4);
        assert_eq!(rb.pop_ready(), None);
    }

    #[test]
    fn single_item() {
        let mut rb = ReorderBuffer::with_capacity(4);
        rb.push(0, 42);
        assert_eq!(rb.pop_ready(), Some(42));
        assert_eq!(rb.pop_ready(), None);
    }

    #[test]
    fn gap_blocks_then_fills() {
        let mut rb = ReorderBuffer::with_capacity(4);

        rb.push(0, "x");
        rb.push(2, "z");

        // 0 is ready, but 1 is missing so 2 is blocked
        assert_eq!(rb.pop_ready(), Some("x"));
        assert_eq!(rb.pop_ready(), None);

        // Fill the gap
        rb.push(1, "y");
        assert_eq!(rb.pop_ready(), Some("y"));
        assert_eq!(rb.pop_ready(), Some("z"));
        assert_eq!(rb.pop_ready(), None);
    }

    /// The bound is the point of the field: a producer that runs away from
    /// a stalled slot must be stopped here, not by whatever accounting the
    /// caller happens to do.
    #[test]
    #[should_panic(expected = "reorder buffer window exceeded")]
    fn push_beyond_max_window_panics() {
        let mut rb = ReorderBuffer::with_max_window(4, 4);
        rb.push(0, "a");
        // seq 4 is 4 slots ahead of next_seq 0, one past the window.
        rb.push(4, "e");
    }

    #[test]
    fn push_at_the_window_edge_is_accepted() {
        let mut rb = ReorderBuffer::with_max_window(4, 4);
        rb.push(3, "d");
        assert_eq!(rb.pending_len(), 4);
        // Draining moves the window forward, so what was out of range
        // becomes reachable rather than permanently rejected.
        rb.push(0, "a");
        assert_eq!(rb.pop_ready(), Some("a"));
        rb.push(4, "e");
        assert_eq!(rb.filled_len(), 2);
    }

    #[test]
    fn filled_diverges_from_window_across_gap() {
        let mut rb = ReorderBuffer::with_capacity(4);

        rb.push(0, "x");
        rb.push(2, "z");
        assert_eq!(rb.filled_len(), 2);
        assert_eq!(rb.pending_len(), 3);

        assert_eq!(rb.pop_ready(), Some("x"));
        assert_eq!(rb.filled_len(), 1);
        assert_eq!(rb.pending_len(), 2);

        rb.push(1, "y");
        assert_eq!(rb.pop_ready(), Some("y"));
        assert_eq!(rb.pop_ready(), Some("z"));
        assert_eq!(rb.filled_len(), 0);
        assert_eq!(rb.pending_len(), 0);
    }
}
