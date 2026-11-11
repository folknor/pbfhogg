//! The collecting diagnostic sink: one per call, and one for setup and the
//! harness's own events. Each event is stamped on the marker FIFO's clock
//! when it is captured.
//!
//! A call's collection closes once every value of the call is dropped and
//! its work has returned (`brokkr man library`, The artefacts: the cut-off
//! is the document). An event that arrives after that is a defect of the
//! harness: it is not kept, it is counted, and the scenario's declared
//! `call-diagnostics-closed` assertion fails on it. At collection close the
//! runner requires exclusive ownership of the call's collector: a retained
//! sender fails the scenario even before it emits anything. Late-event
//! counts supplement that check, and events refused by the collection
//! budget fail the same assertion.

use pbfhogg::debug::MarkerFifo;
use pbfhogg::host::{Event, EventKind};
use std::fmt::Write as _;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

// Harness-local collection policy, like the input limits in json.rs. These
// bounds keep even JSON's six-byte escaping below the artefact size ceiling.
const EVENT_MAX: usize = 256;
const TEXT_BUDGET: usize = 1024 * 1024;

/// Formatting stops at the byte limit, including across write_str calls.
/// Returning fmt::Error stops ordinary Display implementations before they
/// produce the rest of a large message. A Display implementation can still
/// allocate internally or ignore this error; the sink cannot bound that work.
struct BoundedMessage {
    text: String,
    truncated: bool,
}

impl std::fmt::Write for BoundedMessage {
    fn write_str(&mut self, text: &str) -> std::fmt::Result {
        if self.truncated {
            return Err(std::fmt::Error);
        }
        let remaining = crate::protocol::DETAIL_MAX - self.text.len();
        let (prefix, truncated) = crate::protocol::cut(text, remaining);
        self.text.push_str(prefix);
        self.truncated = truncated;
        if truncated {
            Err(std::fmt::Error)
        } else {
            Ok(())
        }
    }
}

fn message(args: std::fmt::Arguments<'_>) -> BoundedMessage {
    let mut output = BoundedMessage {
        text: String::new(),
        truncated: false,
    };
    // A formatter's own error also means that its message is incomplete.
    if output.write_fmt(args).is_err() {
        output.truncated = true;
    }
    output
}

fn key(text: &str) -> String {
    let mut output = String::new();
    for character in text.chars() {
        let character = if character.is_control() {
            ' '
        } else {
            character
        };
        if output.len() + character.len_utf8() > crate::protocol::KEY_MAX {
            break;
        }
        output.push(character);
    }
    if output.is_empty() {
        output.push('-');
    }
    output
}

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
    text_bytes: usize,
    dropped: u64,
}

impl Collector {
    pub(crate) fn new(clock: MarkerFifo) -> Self {
        Self {
            clock,
            state: Mutex::new(State {
                open: true,
                events: Vec::new(),
                text_bytes: 0,
                dropped: 0,
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

    /// Events refused by the collection budget. The runner must fail the
    /// scenario when this is nonzero, for setup as well as call collectors;
    /// truncated evidence must never silently certify a scenario.
    pub(crate) fn dropped(&self) -> u64 {
        self.state().dropped
    }

    /// Events that arrived after [`close`](Self::close).
    pub(crate) fn late(&self) -> u64 {
        self.late.load(Ordering::Relaxed)
    }
}

impl pbfhogg::host::DiagnosticSink for Collector {
    fn event(&self, event: &Event<'_>) {
        let mut state = self.state();
        if !state.open {
            self.late.fetch_add(1, Ordering::Relaxed);
            return;
        }
        // Reserve the worst case before invoking Display. Once saturated,
        // later events do no formatting, allocation or clock reads.
        if state.events.len() >= EVENT_MAX
            || state.text_bytes + crate::protocol::DETAIL_MAX + crate::protocol::KEY_MAX
                > TEXT_BUDGET
        {
            state.dropped = state.dropped.saturating_add(1);
            return;
        }
        let at_us = self.clock.now_us();
        let message = message(event.args);
        let key = key(event.key);
        state.text_bytes += message.text.len() + key.len();
        state.events.push(Captured {
            at_us,
            kind: kind_name(event.kind),
            key,
            message: message.text,
            truncated: message.truncated,
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

    #[test]
    fn oversized_formatting_stops_before_the_next_piece() {
        struct Pieces(std::cell::Cell<usize>);
        impl std::fmt::Display for Pieces {
            fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                for _ in 0..10_000 {
                    self.0.set(self.0.get() + 1);
                    output.write_str("abcd")?;
                }
                Ok(())
            }
        }
        let pieces = Pieces(std::cell::Cell::new(0));
        let kept = super::message(format_args!("{pieces}"));
        assert_eq!(kept.text.len(), crate::protocol::DETAIL_MAX);
        assert!(kept.truncated);
        assert_eq!(pieces.0.get(), crate::protocol::DETAIL_MAX / 4 + 1);
        let kept = super::message(format_args!("{}{}tail", "a".repeat(3999), "\u{e9}"));
        assert_eq!(kept.text.len(), 3999);
        assert!(kept.truncated);
    }

    #[test]
    fn collection_budget_drops_without_formatting_and_survives_close() {
        struct NeverFormat;
        impl std::fmt::Display for NeverFormat {
            fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                panic!("an overflow event must not be formatted");
            }
        }
        let (_dir, fifo, _reader) = crate::test_scratch::marker_fifo();
        let collector = Collector::new(fifo);
        for _ in 0..super::EVENT_MAX {
            note(&collector, "note");
        }
        for _ in 0..1000 {
            collector.event(&Event {
                kind: EventKind::Note,
                key: "overflow",
                args: format_args!("{NeverFormat}"),
            });
        }
        assert_eq!(collector.dropped(), 1000);
        assert_eq!(collector.close().len(), super::EVENT_MAX);
        assert_eq!(collector.dropped(), 1000);
        note(&collector, "late");
        assert_eq!(collector.late(), 1);
        assert_eq!(collector.dropped(), 1000);
    }

    #[test]
    fn bounded_keys_preserve_one_line_and_character_boundaries() {
        assert_eq!(super::key("a\nb\tc"), "a b c");
        assert_eq!(super::key(""), "-");
        let text = format!("{}\u{e9}", "a".repeat(crate::protocol::KEY_MAX - 1));
        assert_eq!(super::key(&text).len(), crate::protocol::KEY_MAX - 1);
    }

    #[test]
    fn text_budget_bounds_large_events_and_keys() {
        let (_dir, fifo, _reader) = crate::test_scratch::marker_fifo();
        let collector = Collector::new(fifo);
        let large = "x".repeat(crate::protocol::DETAIL_MAX);
        let long_key: &'static str = "k".repeat(crate::protocol::KEY_MAX + 10).leak();
        for _ in 0..super::EVENT_MAX {
            collector.event(&Event {
                kind: EventKind::Note,
                key: long_key,
                args: format_args!("{large}"),
            });
        }
        let kept = collector.close();
        assert!(collector.dropped() > 0);
        assert!(kept.len() < super::EVENT_MAX);
        assert!(
            kept.iter()
                .all(|event| event.key.len() == crate::protocol::KEY_MAX)
        );
        assert!(
            kept.iter()
                .map(|event| event.key.len() + event.message.len())
                .sum::<usize>()
                <= super::TEXT_BUDGET
        );
    }
}
