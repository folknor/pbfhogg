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

/// An item [`ReorderBuffer::try_push`] refused, handed back with the reason.
pub(crate) struct Rejected<T> {
    pub(crate) item: T,
    pub(crate) reason: String,
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
    /// A stale, out-of-window or duplicate seq panics deliberately: each
    /// is an upstream programming error (a retry re-sending a seq, a wrong
    /// seq source), not a recoverable runtime condition, and a hard failure
    /// beats silent drift. A caller whose held items must be discharged even
    /// then uses [`try_push`](Self::try_push).
    pub(crate) fn push(&mut self, seq: usize, item: T) {
        if let Err(rejected) = self.try_push(seq, item) {
            panic!("{}", rejected.reason);
        }
    }

    /// As [`push`](Self::push), but a stale, out-of-window or duplicate seq
    /// is returned to the caller together with the item instead of
    /// panicking. For a consumer whose items carry obligations that must be
    /// discharged even on a programming error - the gzip writer's flush
    /// barriers each hold an ack a blocked producer is waiting on, and a
    /// panic would drop it during the unwind, before the failure is on
    /// record.
    pub(crate) fn try_push(&mut self, seq: usize, item: T) -> Result<(), Rejected<T>> {
        if seq < self.next_seq {
            return Err(Rejected {
                item,
                reason: format!(
                    "reorder buffer received stale sequence number: {seq} < {}",
                    self.next_seq
                ),
            });
        }
        let slot_idx = seq - self.next_seq;
        if slot_idx >= self.max_window {
            return Err(Rejected {
                item,
                reason: format!(
                    "reorder buffer window exceeded: seq {seq} is {slot_idx} ahead of \
                     {} (max {})",
                    self.next_seq, self.max_window
                ),
            });
        }
        if slot_idx >= self.pending.len() {
            self.pending.resize_with(slot_idx + 1, || None);
        }
        if self.pending[slot_idx].is_some() {
            return Err(Rejected {
                item,
                reason: format!("duplicate sequence number: {seq}"),
            });
        }
        self.pending[slot_idx] = Some(item);
        self.filled += 1;
        Ok(())
    }

    /// Remove and return every filled slot, front to back, leaving the
    /// buffer empty. Nothing drained counts as delivered: `next_seq` does
    /// not move. For a consumer abandoning ordered delivery after a
    /// failure, which still owes each held item some cleanup.
    pub(crate) fn drain_filled(&mut self) -> impl Iterator<Item = T> + '_ {
        self.filled = 0;
        self.pending.drain(..).flatten()
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

    /// Every refusal hands the item back, so a caller can discharge whatever
    /// the item owes, and leaves the buffer as it was.
    #[test]
    #[allow(clippy::unwrap_used)]
    fn try_push_returns_the_item_on_every_refusal() {
        let mut rb = ReorderBuffer::with_max_window(2, 2);
        rb.try_push(0, "a").ok().unwrap();
        assert_eq!(rb.pop_ready(), Some("a"));

        let stale = rb.try_push(0, "stale").err().unwrap();
        assert_eq!(stale.item, "stale");
        assert!(stale.reason.contains("stale sequence number"));

        let ahead = rb.try_push(3, "ahead").err().unwrap();
        assert_eq!(ahead.item, "ahead");
        assert!(ahead.reason.contains("window exceeded"));

        rb.try_push(2, "c").ok().unwrap();
        let dup = rb.try_push(2, "dup").err().unwrap();
        assert_eq!(dup.item, "dup");
        assert!(dup.reason.contains("duplicate sequence number"));

        assert_eq!(rb.filled_len(), 1);
        rb.try_push(1, "b").ok().unwrap();
        assert_eq!(rb.pop_ready(), Some("b"));
        assert_eq!(rb.pop_ready(), Some("c"));
    }

    /// Draining yields every held item in seq order, skips the gaps, and
    /// does not count anything as delivered.
    #[test]
    fn drain_filled_empties_without_advancing() {
        let mut rb = ReorderBuffer::with_capacity(4);
        rb.push(0, "a");
        assert_eq!(rb.pop_ready(), Some("a"));
        rb.push(2, "c");
        rb.push(4, "e");
        let drained: Vec<_> = rb.drain_filled().collect();
        assert_eq!(drained, vec!["c", "e"]);
        assert_eq!(rb.filled_len(), 0);
        assert_eq!(rb.pending_len(), 0);
        assert_eq!(rb.next_seq(), 1);
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
