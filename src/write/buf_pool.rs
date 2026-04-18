//! Bounded free-list pool for `Vec<u8>` block buffers.
//!
//! The snapshot path in `time_filter` allocates a fresh ~500 KB `Vec<u8>` per
//! block built by `BlockBuilder::take_owned` (see
//! `src/commands/time_filter.rs`). On planet-scale input that is tens of
//! gigabytes of allocation churn feeding glibc arena fragmentation and
//! driving peak anon RSS far above the working set size. The pool recycles
//! the `Vec`s end-to-end: worker pulls a cleared `Vec` with retained
//! capacity, hands it to `BlockBuilder` via `take_owned_swap`, passes the
//! filled `Vec` to the writer's pooled emit path, which sends it back after
//! the rayon compression closure finishes with it.
//!
//! Shape: `Arc<Mutex<Vec<Vec<u8>>>>` - simple stack. The mutex is touched
//! twice per block (one `get` on the worker, one `put` on the writer's
//! rayon thread) which is low frequency relative to the ~5 ms per-block
//! budget at current parallelism. A lock-free queue would be marginal.

use std::sync::Mutex;

/// Maximum retained `Vec<u8>` buffers in the pool. Extras are dropped to
/// bound worst-case memory. 128 is well above the expected in-flight
/// cardinality (~64 blocks per batch + writer's reorder buffer depth),
/// which means steady state runs hot with zero drops.
const POOL_CAPACITY: usize = 128;

/// Bounded free-list pool for `Vec<u8>` block buffers.
pub(crate) struct BlockBufPool {
    stack: Mutex<Vec<Vec<u8>>>,
}

impl BlockBufPool {
    pub(crate) fn new() -> Self {
        Self {
            stack: Mutex::new(Vec::with_capacity(POOL_CAPACITY)),
        }
    }

    /// Get a cleared `Vec<u8>` - from the pool if one is available,
    /// otherwise freshly allocated.
    pub(crate) fn get(&self) -> Vec<u8> {
        let mut guard = self.stack.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.pop().unwrap_or_default()
    }

    /// Return a `Vec<u8>` to the pool. Clears the contents first. If the
    /// pool is at capacity, the `Vec` is dropped.
    pub(crate) fn put(&self, mut v: Vec<u8>) {
        v.clear();
        let mut guard = self.stack.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if guard.len() < POOL_CAPACITY {
            guard.push(v);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn pool_get_returns_empty_by_default() {
        let pool = BlockBufPool::new();
        let v = pool.get();
        assert!(v.is_empty());
        assert_eq!(v.capacity(), 0);
    }

    #[test]
    fn pool_recycles_capacity() {
        let pool = BlockBufPool::new();
        let mut v = Vec::with_capacity(4096);
        v.extend_from_slice(&[1, 2, 3, 4]);
        pool.put(v);
        let v2 = pool.get();
        assert!(v2.is_empty());
        assert_eq!(v2.capacity(), 4096);
    }

    #[test]
    fn pool_drops_at_capacity() {
        let pool = BlockBufPool::new();
        for _ in 0..POOL_CAPACITY + 10 {
            pool.put(Vec::with_capacity(1));
        }
        let guard = pool.stack.lock().unwrap();
        assert_eq!(guard.len(), POOL_CAPACITY);
    }
}
