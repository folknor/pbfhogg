//! The collecting diagnostic sink: one per call, and one for setup and the
//! harness's own events. Each event is stamped on the marker FIFO's clock
//! when it is captured.
//!
//! A call's collection closes once every value of the call is dropped and
//! its work has returned (`brokkr man library`, The artefacts: the cut-off
//! is the document). An event that arrives after that is a defect of the
//! harness: it is not kept, it is counted, and the scenario's declared
//! `call-diagnostics-closed` assertion fails on it. That assertion detects a
//! late event; what prevents one is the runner's order, not the count.

use pbfhogg::debug::MarkerFifo;
use pbfhogg::host::{Event, EventKind};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// One captured event.
pub(crate) struct Captured {
    at_us: u64,
    kind: &'static str,
    key: String,
    message: String,
    truncated: bool,
}

impl Captured {
    pub(crate) fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "at_us": self.at_us,
            "kind": self.kind,
            "key": self.key,
            "message": self.message,
            "truncated": self.truncated,
        })
    }
}

/// The protocol's name for an event kind: the kind's name in lower case.
/// `EventKind` is non-exhaustive; a kind added later is reported as
/// `other` until it is named here.
fn kind_name(kind: EventKind) -> &'static str {
    match kind {
        EventKind::Progress => "progress",
        EventKind::Warning => "warning",
        EventKind::Note => "note",
        EventKind::Summary => "summary",
        EventKind::Developer => "developer",
        _ => "other",
    }
}

pub(crate) struct Collector {
    clock: MarkerFifo,
    state: Mutex<State>,
    late: AtomicU64,
}

struct State {
    open: bool,
    events: Vec<Captured>,
}

impl Collector {
    pub(crate) fn new(clock: MarkerFifo) -> Self {
        Self {
            clock,
            state: Mutex::new(State {
                open: true,
                events: Vec::new(),
            }),
            late: AtomicU64::new(0),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Close the collection and take what it holds, in capture order. An
    /// event admitted before the close is kept; one after it is counted.
    pub(crate) fn close(&self) -> Vec<Captured> {
        let mut state = self.state();
        state.open = false;
        std::mem::take(&mut state.events)
    }

    /// Events that arrived after [`close`](Self::close).
    pub(crate) fn late(&self) -> u64 {
        self.late.load(Ordering::Relaxed)
    }
}

impl pbfhogg::host::DiagnosticSink for Collector {
    fn event(&self, event: &Event<'_>) {
        let at_us = self.clock.now_us();
        let message = event.args.to_string();
        let (kept, truncated) = crate::protocol::cut(&message, crate::protocol::DETAIL_MAX);
        let kept = kept.to_owned();
        let key = crate::protocol::one_line(event.key, crate::protocol::KEY_MAX);
        let key = if key.is_empty() { "-".to_owned() } else { key };
        let mut state = self.state();
        if !state.open {
            self.late.fetch_add(1, Ordering::Relaxed);
            return;
        }
        state.events.push(Captured {
            at_us,
            kind: kind_name(event.kind),
            key,
            message: kept,
            truncated,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::Collector;
    use pbfhogg::host::{DiagnosticSink as _, Event, EventKind};

    fn note(collector: &Collector, key: &'static str) {
        collector.event(&Event {
            kind: EventKind::Note,
            key,
            args: format_args!("a note"),
        });
    }

    /// An event admitted before the close is kept; one after it is counted
    /// as late and never kept.
    #[test]
    fn events_after_close_are_counted_not_kept() {
        let (_dir, fifo, _reader) = crate::test_scratch::marker_fifo();
        let collector = Collector::new(fifo);
        note(&collector, "before");
        let kept = collector.close();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].to_json()["kind"], "note");
        assert_eq!(kept[0].to_json()["key"], "before");
        assert_eq!(collector.late(), 0);
        note(&collector, "after");
        assert_eq!(collector.late(), 1);
        assert!(collector.close().is_empty());
    }
}
