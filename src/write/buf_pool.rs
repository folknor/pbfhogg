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
use std::sync::atomic::{AtomicU64, Ordering};

/// Maximum retained `Vec<u8>` buffers in the pool. Extras are dropped to
/// bound worst-case memory. 128 is well above the expected in-flight
/// cardinality (~64 blocks per batch + writer's reorder buffer depth),
/// which means steady state runs hot with zero drops.
const POOL_CAPACITY: usize = 128;

/// Target minimum capacity for `Vec`s returned from `get`. Fresh allocs
/// are grown to this size before handoff. Pool hits below this are also
/// grown. Measured average block size on Japan is ~136 KB; we pre-size
/// to 512 KB to comfortably cover both nodes (typical 500 KB) and
/// smaller ways/relations blobs without any per-block grow/realloc.
const TARGET_CAPACITY: usize = 512 * 1024;

/// Bounded free-list pool for `Vec<u8>` block buffers.
pub(crate) struct BlockBufPool {
    stack: Mutex<Vec<Vec<u8>>>,
    // Instrumentation: hit/miss/put counters + sum of capacities for
    // average-capacity reporting. Emitted via emit_counters().
    gets_total: AtomicU64,
    gets_hit: AtomicU64,
    gets_hit_capacity_bytes: AtomicU64,
    puts_total: AtomicU64,
    puts_dropped_full: AtomicU64,
    puts_capacity_bytes: AtomicU64,
    puts_len_bytes: AtomicU64,
}

impl BlockBufPool {
    pub(crate) fn new() -> Self {
        Self {
            stack: Mutex::new(Vec::with_capacity(POOL_CAPACITY)),
            gets_total: AtomicU64::new(0),
            gets_hit: AtomicU64::new(0),
            gets_hit_capacity_bytes: AtomicU64::new(0),
            puts_total: AtomicU64::new(0),
            puts_dropped_full: AtomicU64::new(0),
            puts_capacity_bytes: AtomicU64::new(0),
            puts_len_bytes: AtomicU64::new(0),
        }
    }

    /// Get a cleared `Vec<u8>` - from the pool if one is available,
    /// otherwise freshly allocated. Always returned with at least
    /// `TARGET_CAPACITY` to avoid per-block grows in the encoder.
    pub(crate) fn get(&self) -> Vec<u8> {
        self.gets_total.fetch_add(1, Ordering::Relaxed);
        let mut v = {
            let mut guard = self.stack.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(v) = guard.pop() {
                self.gets_hit.fetch_add(1, Ordering::Relaxed);
                self.gets_hit_capacity_bytes
                    .fetch_add(v.capacity() as u64, Ordering::Relaxed);
                v
            } else {
                Vec::new()
            }
        };
        if v.capacity() < TARGET_CAPACITY {
            v.reserve(TARGET_CAPACITY);
        }
        v
    }

    /// Return a `Vec<u8>` to the pool. Clears the contents first. If the
    /// pool is at capacity, the `Vec` is dropped.
    pub(crate) fn put(&self, mut v: Vec<u8>) {
        self.puts_len_bytes
            .fetch_add(v.len() as u64, Ordering::Relaxed);
        v.clear();
        self.puts_total.fetch_add(1, Ordering::Relaxed);
        self.puts_capacity_bytes
            .fetch_add(v.capacity() as u64, Ordering::Relaxed);
        let mut guard = self.stack.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if guard.len() < POOL_CAPACITY {
            guard.push(v);
        } else {
            self.puts_dropped_full.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Emit sidecar counters for the pool's lifecycle.
    pub(crate) fn emit_counters(&self, prefix: &str) {
        use std::sync::atomic::Ordering::Relaxed;
        #[allow(clippy::cast_possible_wrap)]
        {
            crate::debug::emit_counter(&format!("{prefix}_gets_total"), self.gets_total.load(Relaxed) as i64);
            crate::debug::emit_counter(&format!("{prefix}_gets_hit"), self.gets_hit.load(Relaxed) as i64);
            crate::debug::emit_counter(&format!("{prefix}_gets_hit_capacity_bytes"), self.gets_hit_capacity_bytes.load(Relaxed) as i64);
            crate::debug::emit_counter(&format!("{prefix}_puts_total"), self.puts_total.load(Relaxed) as i64);
            crate::debug::emit_counter(&format!("{prefix}_puts_dropped_full"), self.puts_dropped_full.load(Relaxed) as i64);
            crate::debug::emit_counter(&format!("{prefix}_puts_capacity_bytes"), self.puts_capacity_bytes.load(Relaxed) as i64);
            crate::debug::emit_counter(&format!("{prefix}_puts_len_bytes"), self.puts_len_bytes.load(Relaxed) as i64);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn pool_get_pregrows_to_target() {
        let pool = BlockBufPool::new();
        let v = pool.get();
        assert!(v.is_empty());
        assert!(v.capacity() >= TARGET_CAPACITY);
    }

    #[test]
    fn pool_recycles_capacity_above_target() {
        let pool = BlockBufPool::new();
        let mut v = Vec::with_capacity(TARGET_CAPACITY * 2);
        v.extend_from_slice(&[1, 2, 3, 4]);
        pool.put(v);
        let v2 = pool.get();
        assert!(v2.is_empty());
        assert_eq!(v2.capacity(), TARGET_CAPACITY * 2);
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
