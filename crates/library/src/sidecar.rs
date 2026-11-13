//! Harness FIFO wire reader, ported independently from brokkr.
/// A phase marker emitted by the target process.
#[derive(Debug, Clone)]
pub struct Marker {
    /// Arrival order across every stream of one run.
    pub marker_idx: i32,
    pub timestamp_us: i64,
    pub name: String,
    /// The call stream (`%<call_id>` on the wire), or `None` for the
    /// process-wide stream.
    pub call: Option<String>,
}

/// Longest call id the FIFO accepts.
pub(crate) const MAX_CALL_ID_BYTES: usize = 64;

/// A call id is 1..=64 bytes of `[A-Za-z0-9_.-]`, so it needs no escaping on
/// the wire, in JSON or on the command line.
pub(crate) fn is_valid_call_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_CALL_ID_BYTES
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

/// Longest sealing-emission name: the snapshot counter name domain of a
/// library call document, so every sealing record fits the producer's line.
pub(crate) const MAX_SEALING_NAME_BYTES: usize = 400;

/// How a call sink ended, as its terminal record says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SinkEnd {
    Sealed,
    Failed,
}

impl SinkEnd {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Sealed => "sealed",
            Self::Failed => "failed",
        }
    }
}

/// One parsed FIFO record.
#[derive(Debug, PartialEq, Eq)]
enum FifoRecord<'a> {
    Blank,
    Rejected,
    Marker {
        ts: i64,
        call: Option<&'a str>,
        name: &'a str,
    },
    Counter {
        ts: i64,
        call: Option<&'a str>,
        name: &'a str,
        value: i64,
    },
    /// `%CALL @@NAME=VALUE`: one final counter value a sealing sink emits.
    Sealing {
        ts: i64,
        call: &'a str,
        name: &'a str,
        value: i64,
    },
    /// `%CALL !sealed drops=N` or `%CALL !failed drops=N`: a sink's last line.
    Terminal {
        ts: i64,
        call: &'a str,
        end: SinkEnd,
        drops: u64,
    },
}

impl FifoRecord<'_> {
    /// The call a tagged record names.
    fn call(&self) -> Option<&str> {
        match self {
            Self::Marker { call, .. } | Self::Counter { call, .. } => *call,
            Self::Sealing { call, .. } | Self::Terminal { call, .. } => Some(call),
            Self::Blank | Self::Rejected => None,
        }
    }

    /// The record to keep; a marker takes `marker_idx`. `None` for a line
    /// that carries none.
    fn into_record(self, marker_idx: i32) -> Option<Record> {
        Some(match self {
            Self::Blank | Self::Rejected => return None,
            Self::Marker { ts, call, name } => Record::Marker(Marker {
                marker_idx,
                timestamp_us: ts,
                name: name.to_owned(),
                call: call.map(str::to_owned),
            }),
            Self::Counter {
                ts,
                call,
                name,
                value,
            } => Record::Counter(Counter {
                timestamp_us: ts,
                name: name.to_owned(),
                value,
                call: call.map(str::to_owned),
            }),
            Self::Sealing {
                ts,
                call,
                name,
                value,
            } => Record::Sealing(Sealing {
                timestamp_us: ts,
                call: call.to_owned(),
                name: name.to_owned(),
                value,
            }),
            Self::Terminal {
                ts,
                call,
                end,
                drops,
            } => Record::Terminal(Terminal {
                timestamp_us: ts,
                call: call.to_owned(),
                end,
                drops,
            }),
        })
    }
}

/// `N` of a terminal record: a u64 in decimal, without sign or leading zero.
fn parse_drops(text: &str) -> Option<u64> {
    let canonical = !text.is_empty()
        && text.bytes().all(|b| b.is_ascii_digit())
        && (text == "0" || !text.starts_with('0'));
    if canonical { text.parse().ok() } else { None }
}

/// The body of a tagged record that starts `@@` or `!`, which only sealing
/// sinks write; anything else of those shapes is rejected.
fn parse_sink_end<'a>(ts: i64, call: &'a str, body: &'a str) -> Option<FifoRecord<'a>> {
    if let Some(sealing) = body.strip_prefix("@@") {
        let (name, value) = sealing.split_once('=')?;
        if name.is_empty() || name.len() > MAX_SEALING_NAME_BYTES {
            return None;
        }
        return Some(FifoRecord::Sealing {
            ts,
            call,
            name,
            value: value.parse().ok()?,
        });
    }
    let terminal = body.strip_prefix('!')?;
    let (end, drops) = match terminal.strip_prefix("sealed drops=") {
        Some(n) => (SinkEnd::Sealed, n),
        None => (SinkEnd::Failed, terminal.strip_prefix("failed drops=")?),
    };
    Some(FifoRecord::Terminal {
        ts,
        call,
        end,
        drops: parse_drops(drops)?,
    })
}

/// Parse one FIFO line. `<ts> <name>` and `<ts> @<name>=<i64>` are
/// process-wide; `<ts> %<call_id> <name>` and `<ts> %<call_id> @<name>=<i64>`
/// belong to that call. A payload starting with `%` is only ever a tagged
/// record: a malformed one is rejected, never read as an untagged marker. In
/// a tagged record a body starting `@@` is a sealing emission and one
/// starting `!` a terminal record; untagged lines parse as they always did.
fn parse_fifo_record(line: &str) -> FifoRecord<'_> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return FifoRecord::Blank;
    }
    let Some((ts_str, payload)) = trimmed.split_once(' ') else {
        return FifoRecord::Rejected;
    };
    let Ok(ts) = ts_str.parse::<i64>() else {
        return FifoRecord::Rejected;
    };
    let (call, body) = if let Some(tagged) = payload.strip_prefix('%') {
        let Some((call, body)) = tagged.split_once(' ') else {
            return FifoRecord::Rejected;
        };
        if !is_valid_call_id(call) || body.is_empty() {
            return FifoRecord::Rejected;
        }
        if body.starts_with("@@") || body.starts_with('!') {
            return parse_sink_end(ts, call, body).unwrap_or(FifoRecord::Rejected);
        }
        (Some(call), body)
    } else {
        (None, payload)
    };
    if let Some(counter_body) = body.strip_prefix('@') {
        return match counter_body.split_once('=') {
            Some((name, value)) if !name.is_empty() => match value.parse::<i64>() {
                Ok(value) => FifoRecord::Counter {
                    ts,
                    call,
                    name,
                    value,
                },
                Err(_) => FifoRecord::Rejected,
            },
            _ => FifoRecord::Rejected,
        };
    }
    FifoRecord::Marker {
        ts,
        call,
        name: body,
    }
}

/// An application-level counter emitted by the target process.
#[derive(Debug, Clone)]
pub struct Counter {
    pub timestamp_us: i64,
    pub name: String,
    pub value: i64,
    /// As on [`Marker`]: `None` is the process-wide stream.
    pub call: Option<String>,
}

/// One final counter value a call's sink emitted while sealing,
/// `%CALL @@NAME=VALUE` on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Sealing {
    pub timestamp_us: i64,
    pub call: String,
    pub name: String,
    pub value: i64,
}

/// A call sink's terminal record, `%CALL !sealed drops=N` on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Terminal {
    pub timestamp_us: i64,
    pub call: String,
    pub end: SinkEnd,
    /// Lines the sink dropped before this one.
    pub drops: u64,
}

/// One captured FIFO record. A capture keeps them in one list in arrival
/// order, so the order between a marker, a counter and a sink's last lines
/// is never lost.
#[derive(Debug, Clone)]
pub(crate) enum Record {
    Marker(Marker),
    Counter(Counter),
    Sealing(Sealing),
    Terminal(Terminal),
}

impl Record {
    /// The call stream it belongs to; `None` is the process-wide stream.
    pub(crate) fn call(&self) -> Option<&str> {
        match self {
            Self::Marker(m) => m.call.as_deref(),
            Self::Counter(c) => c.call.as_deref(),
            Self::Sealing(s) => Some(&s.call),
            Self::Terminal(t) => Some(&t.call),
        }
    }

    pub(crate) fn timestamp_us(&self) -> i64 {
        match self {
            Self::Marker(m) => m.timestamp_us,
            Self::Counter(c) => c.timestamp_us,
            Self::Sealing(s) => s.timestamp_us,
            Self::Terminal(t) => t.timestamp_us,
        }
    }

    /// The record in its wire spelling, without the newline.
    pub(crate) fn wire(&self) -> String {
        let tag = self.call().map_or_else(String::new, |c| format!("%{c} "));
        let ts = self.timestamp_us();
        match self {
            Self::Marker(m) => format!("{ts} {tag}{}", m.name),
            Self::Counter(c) => format!("{ts} {tag}@{}={}", c.name, c.value),
            Self::Sealing(s) => format!("{ts} {tag}@@{}={}", s.name, s.value),
            Self::Terminal(t) => format!("{ts} {tag}!{} drops={}", t.end.name(), t.drops),
        }
    }
}

/// What a capture omitted, by cause. A record the FIFO reader may not hold
/// (`stream_lost`) is a different fact from a sample or output byte it did
/// not keep: a library verification's call-stream verdict reads the first
/// only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct CaptureLoss {
    /// FIFO lines the reader refused: malformed, overlong, invalid UTF-8,
    /// or a fragment left without a newline when the writers closed.
    pub rejected_lines: usize,
    /// A FIFO read failed, or the last drain's byte budget left records
    /// possibly unread.
    pub fifo_unread: bool,
    /// The marker or counter budget dropped a parsed record.
    pub records_capped: bool,
}

impl CaptureLoss {
    /// Some FIFO record may be missing from the capture.
    pub(crate) fn stream_lost(&self) -> bool {
        self.rejected_lines > 0 || self.fifo_unread || self.records_capped
    }

    /// Each cause that may have lost a FIFO record, as a short phrase.
    pub(crate) fn stream_causes(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.rejected_lines > 0 {
            out.push(format!(
                "the FIFO reader rejected {}",
                crate::output::count(self.rejected_lines, "line")
            ));
        }
        if self.fifo_unread {
            out.push("FIFO records may have been left unread".to_owned());
        }
        if self.records_capped {
            out.push("the marker or counter limit dropped records".to_owned());
        }
        out
    }
}

/// Every call id the tagged records of one capture named, whether or not
/// the record budgets kept the record, up to a bound. Past the bound it
/// only says that more ids arrived, so a caller that declared fewer calls
/// than the bound knows some id was not one of them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CallCensus {
    pub ids: std::collections::BTreeSet<String>,
    pub overflowed: bool,
}

impl CallCensus {
    fn note(&mut self, call: &str, limit: usize) {
        if self.ids.contains(call) {
            return;
        }
        if self.ids.len() < limit {
            self.ids.insert(call.to_owned());
        } else {
            self.overflowed = true;
        }
    }
}

/// Parse a bounded captured wire stream, preserving arrival order.
pub(crate) fn parse(
    bytes: &[u8],
    overflow: bool,
    census_limit: usize,
) -> (Vec<Record>, CaptureLoss, CallCensus) {
    let mut loss = CaptureLoss {
        records_capped: overflow,
        ..CaptureLoss::default()
    };
    let mut census = CallCensus::default();
    let mut records = Vec::new();
    for line in bytes.split_inclusive(|b| *b == b'\n') {
        let parsed = if line.ends_with(b"\n")
            && line.len().saturating_sub(1) <= crate::tunables::FIFO_LINE_LIMIT
        {
            std::str::from_utf8(line)
                .map(parse_fifo_record)
                .unwrap_or(FifoRecord::Rejected)
        } else {
            FifoRecord::Rejected
        };
        if parsed == FifoRecord::Rejected {
            loss.rejected_lines += 1;
        }
        if let Some(call) = parsed.call() {
            census.note(call, census_limit);
        }
        if let Some(record) = parsed.into_record(i32::try_from(records.len()).unwrap_or(i32::MAX)) {
            records.push(record);
        }
    }
    (records, loss, census)
}

pub(crate) struct Timeline<'a> {
    markers: Vec<&'a Marker>,
    final_us: Option<i64>,
}
pub(crate) struct Segment {
    pub name: String,
    pub start_us: i64,
    pub end_us: Option<i64>,
}
impl<'a> Timeline<'a> {
    pub(crate) fn open(markers: &'a [Marker]) -> Self {
        let mut markers: Vec<_> = markers.iter().collect();
        markers.sort_by_key(|m| (m.timestamp_us, m.marker_idx));
        Self {
            markers,
            final_us: None,
        }
    }
    pub(crate) fn closed(markers: &'a [Marker], end: i64) -> Self {
        let mut value = Self::open(markers);
        value.final_us = Some(end);
        value
    }
    pub(crate) fn marker_names(&self) -> Vec<&str> {
        self.markers.iter().map(|m| m.name.as_str()).collect()
    }
    pub(crate) fn segments(&self) -> Vec<Segment> {
        self.markers
            .iter()
            .enumerate()
            .map(|(i, m)| Segment {
                name: m.name.clone(),
                start_us: m.timestamp_us,
                end_us: self
                    .markers
                    .get(i + 1)
                    .map(|n| n.timestamp_us)
                    .or(self.final_us),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    #[test]
    fn wire_records_keep_arrival_order_and_call_tags() {
        let (records, loss, census) = parse(
            b"15 %a CAT_START\n25 %a @@n=3\n26 %a !sealed drops=0\n27 @global=2\n",
            false,
            2,
        );
        assert!(!loss.stream_lost());
        assert_eq!(records.len(), 4);
        assert_eq!(records[0].wire(), "15 %a CAT_START");
        assert_eq!(records[1].wire(), "25 %a @@n=3");
        assert_eq!(records[2].wire(), "26 %a !sealed drops=0");
        assert_eq!(records[3].call(), None);
        assert_eq!(census.ids.into_iter().collect::<Vec<_>>(), ["a"]);
    }
    #[test]
    fn fragments_invalid_utf8_overlong_lines_and_bad_tags_are_loss() {
        for bytes in [
            b"12 %bad! PHASE\n".as_slice(),
            b"12 %a !sealed drops=01\n",
            b"12 %a @@=3\n",
            b"12 %a PHASE",
            b"\xff\n",
        ] {
            let (records, loss, _) = parse(bytes, false, 2);
            assert!(records.is_empty());
            assert_eq!(loss.rejected_lines, 1);
        }
        let bytes = vec![b'x'; crate::tunables::FIFO_LINE_LIMIT + 1];
        assert!(parse(&bytes, false, 2).1.stream_lost());
        assert!(parse(b"", true, 2).1.stream_lost());
    }
}
