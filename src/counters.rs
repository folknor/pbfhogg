//! Sidecar counter machinery.
//!
//! Three shared pieces; new counter code uses them rather than hand-rolling
//! its own (some older command modules still do):
//!
//! - [`counter_set!`] declares a set of `AtomicU64` counters from one list
//!   that pairs every field with the exact name it is emitted under, so a
//!   counter is spelled once (field, sidecar name and emission order all come
//!   from the same line) and every sidecar name stays greppable verbatim.
//! - [`record_max`] is the "track the maximum" update.
//! - [`Stopwatch`] turns elapsed time into a `u64` for timed counters.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Instant;

/// Declare a set of sidecar counters.
///
/// ```ignore
/// crate::counters::counter_set! {
///     /// Docs for the set.
///     pub(super) struct StageCounters {
///         /// Docs for one counter.
///         pread_ms => "s9_pread_ms",
///         // Accumulated in nanoseconds, emitted in milliseconds.
///         write_ns => "s9_write_ms" / 1_000_000,
///     }
///     // Optional non-counter fields, with their (const) initialisers.
///     extra {
///         emitted: AtomicBool = AtomicBool::new(false),
///     }
/// }
/// ```
///
/// Generates the struct (one `AtomicU64` per counter, then the extra fields),
/// a `const fn new()` (so a set can back a `static`), `Default`, and two
/// **private** methods: `for_each(|name, value|)`, which visits every counter
/// in declaration order with its emitted name and its value (loaded, then
/// divided by the optional divisor), and `emit_all()`, which publishes that
/// visit through `crate::debug::emit_counter` (saturating at `i64::MAX`).
/// Private inherent methods are reachable only from the module that invokes
/// the macro and its children, so a set whose emission is restricted to one
/// route (the process-exit hooks) keeps that route as its only public way out.
///
/// Attributes on a counter are copied to its field declaration only; do not
/// put `#[cfg]` on a counter (the initialiser and the visit would not see
/// it). A counter that only some builds bump stays declared and reads zero.
macro_rules! counter_set {
    (
        $(#[$sattr:meta])*
        $vis:vis struct $name:ident {
            $(
                $(#[$fattr:meta])*
                $fvis:vis $field:ident => $emit:literal $(/ $div:literal)?
            ),* $(,)?
        }
        $(extra {
            $(
                $(#[$xattr:meta])*
                $xvis:vis $xfield:ident : $xty:ty = $xinit:expr
            ),* $(,)?
        })?
    ) => {
        $(#[$sattr])*
        $vis struct $name {
            $(
                $(#[$fattr])*
                $fvis $field: ::std::sync::atomic::AtomicU64,
            )*
            $($(
                $(#[$xattr])*
                $xvis $xfield: $xty,
            )*)?
        }

        #[allow(dead_code)]
        impl $name {
            $vis const fn new() -> Self {
                Self {
                    $($field: ::std::sync::atomic::AtomicU64::new(0),)*
                    $($($xfield: $xinit,)*)?
                }
            }

            /// Visit every counter in declaration order: its emitted name
            /// and its current value, divided by its divisor if it has one.
            fn for_each(&self, mut visit: impl FnMut(&'static str, u64)) {
                $(
                    visit(
                        $emit,
                        self.$field.load(::std::sync::atomic::Ordering::Relaxed) $(/ $div)?,
                    );
                )*
            }

            /// Emit every counter, in declaration order.
            fn emit_all(&self) {
                self.for_each(|name, value| $crate::debug::emit_counter(name, value));
            }
        }

        impl ::std::default::Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }
    };
}
pub(crate) use counter_set;

/// Raise `field` to `candidate` if `candidate` is larger.
///
/// Hand-written on purpose, not `AtomicU64::fetch_max`. On x86-64 there is no
/// atomic max instruction: `fetch_max` compiles to a `lock cmpxchg` loop that
/// performs the locked read-modify-write on every call, taking the cache line
/// exclusive even when the maximum does not move. This loads first and only
/// attempts the compare-exchange while the candidate is larger, so in steady
/// state (the maximum reached early, later candidates smaller) a call is a
/// plain load and the line can stay shared across the workers reporting into
/// it. When the candidate is larger the two cost the same (`fetch_max` also
/// loads before its loop). The high-water and peak counters on the per-blob
/// read, write and altw paths all go through here; none of them has been
/// measured either way.
#[inline]
pub(crate) fn record_max(field: &AtomicU64, candidate: u64) {
    let mut current = field.load(Relaxed);
    while candidate > current {
        match field.compare_exchange_weak(current, candidate, Relaxed, Relaxed) {
            Ok(_) => break,
            Err(observed) => current = observed,
        }
    }
}

/// A started clock for a timed counter.
///
/// Elapsed time as a `u64`: nanoseconds and milliseconds both saturate at `u64::MAX` (unreachable in practice: about 584 years of
/// nanoseconds). Every method reads the clock afresh and none resets the
/// start, so one stopwatch can feed several counters; a caller that needs the
/// *same* sample in more than one place (a total and a maximum, or two
/// counters that must agree exactly) takes it once with
/// [`elapsed_ns`](Self::elapsed_ns) / [`elapsed_ms`](Self::elapsed_ms) and
/// reuses it.
///
/// Millisecond counters truncate **per sample**: each recording adds whole
/// milliseconds, so a sample under 1 ms adds nothing and a counter fed by
/// many short samples undercounts. That is deliberate continuity - the
/// existing `_ms` counters were kept with this meaning so they stay
/// comparable with earlier result sets, and nanosecond twins were added where
/// resolution mattered. New timed counters should accumulate nanoseconds and,
/// if they must be reported in milliseconds, divide at emission (the
/// `counter_set!` divisor).
#[derive(Clone, Copy, Debug)]
#[must_use]
pub(crate) struct Stopwatch(Instant);

impl Stopwatch {
    #[inline]
    pub(crate) fn start() -> Self {
        Self(Instant::now())
    }

    /// Nanoseconds since the start.
    #[inline]
    pub(crate) fn elapsed_ns(&self) -> u64 {
        u64::try_from(self.0.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }

    /// Whole milliseconds since the start (truncated, see the type docs).
    #[inline]
    pub(crate) fn elapsed_ms(&self) -> u64 {
        u64::try_from(self.0.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// Add the nanoseconds since the start to `into`.
    #[inline]
    pub(crate) fn record_ns(&self, into: &AtomicU64) {
        into.fetch_add(self.elapsed_ns(), Relaxed);
    }

    /// Add the whole milliseconds since the start to `into`.
    #[inline]
    pub(crate) fn record_ms(&self, into: &AtomicU64) {
        into.fetch_add(self.elapsed_ms(), Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};

    super::counter_set! {
        struct Probe {
            plain => "probe_plain",
            scaled_ns => "probe_scaled_ms" / 1_000_000,
        }
        extra {
            latch: AtomicBool = AtomicBool::new(false),
        }
    }

    #[test]
    fn counter_set_visits_in_order_with_divisor() {
        let probe = Probe::new();
        probe.plain.fetch_add(7, Relaxed);
        probe.scaled_ns.fetch_add(3_999_999, Relaxed);
        let mut seen = Vec::new();
        probe.for_each(|name, value| seen.push((name, value)));
        assert_eq!(seen, [("probe_plain", 7), ("probe_scaled_ms", 3)]);
        assert!(!probe.latch.load(Relaxed));
    }

    #[test]
    fn record_max_only_raises() {
        let field = AtomicU64::new(5);
        super::record_max(&field, 3);
        assert_eq!(field.load(Relaxed), 5);
        super::record_max(&field, 9);
        assert_eq!(field.load(Relaxed), 9);
    }

    #[test]
    fn stopwatch_records_without_resetting() {
        let watch = super::Stopwatch::start();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let ns = AtomicU64::new(0);
        let ms = AtomicU64::new(0);
        watch.record_ms(&ms);
        watch.record_ns(&ns);
        assert!(ms.load(Relaxed) >= 2);
        assert!(ns.load(Relaxed) >= 2_000_000);
        assert!(watch.elapsed_ns() >= ns.load(Relaxed));
    }
}
