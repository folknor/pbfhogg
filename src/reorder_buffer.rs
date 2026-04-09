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

        // Push 1 first — can't pop yet (0 missing)
        rb.push(1, "b");
        assert_eq!(rb.pop_ready(), None);

        // Push 0 — now 0 and 1 are contiguous
        rb.push(0, "a");
        assert_eq!(rb.pop_ready(), Some("a"));
        assert_eq!(rb.pop_ready(), Some("b"));

        // Push 2 — immediately ready
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
}
