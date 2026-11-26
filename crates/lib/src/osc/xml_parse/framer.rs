//! The framing reader: cuts an OSC document that is a regular file into
//! spans of contiguous bytes for workers to parse, without parsing it.
//!
//! The reader thread of merge-changes and of OSC tags-filter is the bound of
//! both commands. Tokenizing with quick-xml, decoding UTF-8 and checking
//! characters made up most of that thread (perf, planet daily OSC, bygg,
//! 2026-11-26: about 80 % of its time; recording the transcript about 7 %).
//! A framer only finds where markup begins and ends: it scans for `<`, and
//! inside markup for the byte that ends it, with quick-xml's own
//! [`ElementParser`] (quote-aware, for start and end tags) and
//! [`PiParser`], and quick-xml's rules for the end of a comment and of a
//! CDATA section. Everything else, every check included, happens on the
//! worker that parses the span ([`super::Span::parse`]).
//!
//! # Cut points
//!
//! A span ends, at the first chance past the batch target, before a `<`
//! that begins markup directly inside the root or directly inside a
//! `<create>`, `<modify>` or `<delete>`. There quick-xml holds no state
//! but its open names, which the next span's reader is primed with
//! ([`super::Continuation`]); an object is never split, so a sink's
//! per-element state never crosses a span.
//!
//! # Why a framing mistake cannot become a wrong answer
//!
//! Each span's worker drives the span from its entry continuation to the
//! end of its bytes and must stand exactly at its exit continuation there,
//! with nothing open; the last span must close the root after a clean end
//! of input. A cut quick-xml would not have made ends its span inside
//! markup, which the worker refuses (an unclosed tag, comment or quoted
//! value), or at a different continuation, which it refuses too. So the
//! framer can only cost a refusal or parallelism, never an answer; and on a
//! document quick-xml accepts it makes exactly quick-xml's boundaries.
//!
//! # Fallback
//!
//! At markup the framer does not follow (a DOCTYPE, `<!` other than a
//! comment or CDATA, the empty comment `<!---->` on whose end quick-xml's
//! rule is not certain here), or when a span grows past the ceiling with no
//! cut point, the input is finished by the tokenizing reader: a quick-xml
//! reader primed at the unsealed span's entry continuation reads the span's
//! bytes and then the rest of the input, recording transcripts as a stream
//! does. That is exact and bounded, only serial.

use std::cell::Cell;
use std::io::{self, BufRead, BufReader};
use std::path::Path;
use std::rc::Rc;

use quick_xml::parser::{ElementParser, Parser as _, PiParser};

use super::{
    Continuation, LexBatch, LexSink, OscAction, OscInput, ParseResult, Part, continue_at,
    drive_osc, drive_osc_from, is_xml_space, open_osc_input, with_inflated, xml_reader,
};
use crate::error::Error;
use crate::tunables::OSC_FRAME_FEED_BYTES as FEED;

/// Where a framing reader seals spans.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Framing {
    /// Batch bytes at which the next cut point seals a span (and at which
    /// a stream's transcript is sealed).
    pub(crate) target: usize,
    /// Span bytes past which the input falls back to the tokenizing reader.
    pub(crate) ceiling: usize,
}

/// Read the OSC document at `path` into batches handed to `seal`, in
/// document order, each at most one cut past `framing.target`: spans for a
/// regular file, transcripts for a stream. `halted` is asked at every read
/// and every recorded callback; once it answers true the read refuses.
/// Emits the input-read counter as [`super::parse_osc_path`] does.
pub(crate) fn frame_osc_path<F, H>(
    path: &Path,
    framing: Framing,
    seal: F,
    halted: H,
    cancel: &crate::host::Recorder,
) -> ParseResult<()>
where
    F: FnMut(LexBatch) -> ParseResult<()>,
    H: Fn() -> bool,
{
    let total = Rc::new(Cell::new(0_u64));
    let (result, decompress_total) = match open_osc_input(path, &total, cancel)? {
        // A stream is tokenized on this thread: a read ahead of the parse
        // can block on its writer after the parse has found an error.
        OscInput::Inline {
            read,
            regular: false,
        } => {
            let mut sink = LexSink::new(framing.target, seal, halted);
            let mut reader = xml_reader(BufReader::new(read));
            let result = drive_osc(&mut reader, &mut sink, path);
            let result = sink.finish_after(result);
            (result, total.get())
        }
        OscInput::Inline {
            read,
            regular: true,
        } => {
            let mut source = BufReader::with_capacity(FEED, read);
            let result = Framer::new(framing, seal, halted).run(&mut source, path);
            (result, total.get())
        }
        OscInput::InflateAhead(file) => with_inflated(file, path, cancel, move |source| {
            Framer::new(framing, seal, halted).run(source, path)
        }),
    };
    // The historical per-file counter, once per file, as every OSC read
    // emits it; see `parse_osc_path`.
    cancel.counter("merge_changes_decompress_ns", decompress_total);
    result
}

#[cold]
fn stopped() -> Error {
    Error::internal("OSC reading stopped after a failure elsewhere")
}

/// What the framer is inside of. Every position is an index into the
/// unsealed span.
#[derive(Clone, Copy)]
enum Lex {
    /// Between markup.
    Text,
    /// A `<` at `lt`, the byte after it not yet read.
    Open { lt: usize },
    /// A start tag (`end` false) or end tag from `lt`, to its `>`.
    Tag {
        lt: usize,
        end: bool,
        parser: ElementParser,
    },
    /// A processing instruction, to its `?>`.
    Pi { parser: PiParser },
    /// A comment from `lt`, to its `-->`.
    Comment { lt: usize },
    /// A CDATA section from `lt`, to its `]]>`.
    CData { lt: usize },
}

/// How a scan of the bytes read so far ended.
enum Scan {
    /// Every byte read is accounted for; more are needed.
    More,
    /// Markup the framer does not follow: fall back.
    Anomaly,
}

struct Framer<F, H> {
    framing: Framing,
    seal: F,
    halted: H,
    /// The unsealed span: every byte read since the last cut.
    span: Vec<u8>,
    /// Where the unsealed span starts; `None` at the document's start.
    entry: Option<Continuation>,
    /// Scan position in `span`.
    pos: usize,
    lex: Lex,
    /// Open elements, the root included.
    depth: u32,
    /// The root has closed (or was self-closing): no cut follows.
    root_closed: bool,
    /// The action section open at depth 2, if the depth-1 element open is
    /// one; inside any other element under the root nothing is cut.
    section: Option<OscAction>,
}

impl<F, H> Framer<F, H>
where
    F: FnMut(LexBatch) -> ParseResult<()>,
    H: Fn() -> bool,
{
    fn new(framing: Framing, seal: F, halted: H) -> Self {
        Self {
            framing,
            seal,
            halted,
            span: Vec::new(),
            entry: None,
            pos: 0,
            lex: Lex::Text,
            depth: 0,
            root_closed: false,
            section: None,
        }
    }

    fn run<R: BufRead>(mut self, source: &mut R, path: &Path) -> ParseResult<()> {
        // loop-ends: each pass appends input or ends at end of input, a read
        // failure, a halt, a seal failure or the fallback.
        loop {
            if (self.halted)() {
                return Err(stopped());
            }
            let available = match source.fill_buf() {
                Ok(bytes) => bytes,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if crate::error::io_carries_cancellation(&e) => {
                    return Err(crate::error::new_cancelled());
                }
                // The unsealed bytes may hold a fault a serial parse meets
                // before this one: parse them, the failure at their end.
                Err(e) => return self.fall_back(&mut FailsWith(Some(e)), path),
            };
            if available.is_empty() {
                return self.finish();
            }
            let n = available.len().min(FEED);
            self.span.extend_from_slice(&available[..n]);
            source.consume(n);
            match self.scan()? {
                Scan::More if self.span.len() <= self.framing.ceiling => {}
                Scan::More | Scan::Anomaly => return self.fall_back(source, path),
            }
        }
    }

    /// Account for every byte read so far, sealing a span at each cut
    /// point past the target.
    fn scan(&mut self) -> ParseResult<Scan> {
        loop {
            let len = self.span.len();
            match self.lex {
                Lex::Text => {
                    let Some(i) = memchr::memchr(b'<', &self.span[self.pos..]) else {
                        self.pos = len;
                        return Ok(Scan::More);
                    };
                    let mut lt = self.pos + i;
                    if lt >= self.framing.target
                        && let Some(cut) = self.cut_point()
                    {
                        self.cut(lt, cut)?;
                        lt = 0;
                    }
                    self.lex = Lex::Open { lt };
                    self.pos = lt + 1;
                }
                Lex::Open { lt } => {
                    if let Some(stop) = self.classify(lt) {
                        return Ok(stop);
                    }
                }
                Lex::Tag {
                    lt,
                    end,
                    mut parser,
                } => match parser.feed(&self.span[self.pos..]) {
                    None => {
                        self.lex = Lex::Tag { lt, end, parser };
                        self.pos = len;
                        return Ok(Scan::More);
                    }
                    Some(i) => {
                        let gt = self.pos + i;
                        if end {
                            self.close();
                        } else {
                            self.open(lt, gt);
                        }
                        self.lex = Lex::Text;
                        self.pos = gt + 1;
                    }
                },
                Lex::Pi { mut parser } => match parser.feed(&self.span[self.pos..]) {
                    None => {
                        self.lex = Lex::Pi { parser };
                        self.pos = len;
                        return Ok(Scan::More);
                    }
                    Some(i) => {
                        self.lex = Lex::Text;
                        self.pos += i + 1;
                    }
                },
                Lex::Comment { lt } => {
                    // quick-xml ends a comment at the first `>` after `--`
                    // that lies far enough past the `<`. At exactly
                    // `<!---->` its length rule is not certain here, so the
                    // empty comment falls back rather than risk a boundary
                    // quick-xml does not make.
                    match self.find_end(b"--", lt + 6) {
                        None => return Ok(Scan::More),
                        Some(gt) if gt == lt + 6 => return Ok(Scan::Anomaly),
                        Some(gt) => {
                            self.lex = Lex::Text;
                            self.pos = gt + 1;
                        }
                    }
                }
                Lex::CData { lt } => match self.find_end(b"]]", lt + 11) {
                    None => return Ok(Scan::More),
                    Some(gt) => {
                        self.lex = Lex::Text;
                        self.pos = gt + 1;
                    }
                },
            }
        }
    }

    /// Read what the markup at the `<` at `lt` is, as quick-xml does from
    /// the byte after it, or say why the scan stops there.
    fn classify(&mut self, lt: usize) -> Option<Scan> {
        let Some(&next) = self.span.get(lt + 1) else {
            return Some(Scan::More);
        };
        let (lex, from) = match next {
            b'!' => {
                let have = &self.span[lt..];
                if have.starts_with(b"<!--") {
                    (Lex::Comment { lt }, lt + 4)
                } else if have.starts_with(b"<![CDATA[") {
                    (Lex::CData { lt }, lt + 9)
                } else if b"<!--".starts_with(have) || b"<![CDATA[".starts_with(have) {
                    return Some(Scan::More);
                } else {
                    return Some(Scan::Anomaly);
                }
            }
            b'?' => (
                Lex::Pi {
                    parser: PiParser(false),
                },
                lt + 1,
            ),
            _ => (
                Lex::Tag {
                    lt,
                    end: next == b'/',
                    parser: ElementParser::Outside,
                },
                lt + 1,
            ),
        };
        self.lex = lex;
        self.pos = from;
        None
    }

    /// The first `>` at or past `min` and the scan position that `close`
    /// directly precedes, or `None` with the scan position at the end.
    fn find_end(&mut self, close: &[u8; 2], min: usize) -> Option<usize> {
        let mut from = self.pos;
        // loop-ends: each pass moves `from` past one `>`, or returns.
        loop {
            let Some(i) = memchr::memchr(b'>', &self.span[from..]) else {
                self.pos = self.span.len();
                return None;
            };
            let gt = from + i;
            if gt >= min && &self.span[gt - 2..gt] == close {
                return Some(gt);
            }
            from = gt + 1;
        }
    }

    /// A start tag from `lt` to its `>` at `gt`.
    fn open(&mut self, lt: usize, gt: usize) {
        let content = &self.span[lt + 1..gt];
        // quick-xml reads a tag whose content ends in `/` as self-closing.
        let empty = content.last() == Some(&b'/');
        match self.depth {
            0 if empty => self.root_closed = true,
            1 if !empty => {
                let name_end = content
                    .iter()
                    .position(|&b| is_xml_space(b))
                    .unwrap_or(content.len());
                self.section = match &content[..name_end] {
                    b"create" => Some(OscAction::Create),
                    b"modify" => Some(OscAction::Modify),
                    b"delete" => Some(OscAction::Delete),
                    _ => None,
                };
                self.depth += 1;
            }
            _ if !empty => self.depth += 1,
            _ => {}
        }
    }

    fn close(&mut self) {
        match self.depth {
            0 => {}
            1 => {
                self.depth = 0;
                self.root_closed = true;
            }
            2 => {
                self.depth = 1;
                self.section = None;
            }
            _ => self.depth -= 1,
        }
    }

    /// Where a cut before the `<` now being scanned would stand, if one
    /// may be made there.
    fn cut_point(&self) -> Option<Continuation> {
        if self.root_closed {
            return None;
        }
        match self.depth {
            1 => Some(Continuation::Root),
            2 => self.section.map(Continuation::Section),
            _ => None,
        }
    }

    /// Seal the span's bytes before `lt`; the next span starts at `lt`.
    fn cut(&mut self, lt: usize, at: Continuation) -> ParseResult<()> {
        let mut rest = Vec::with_capacity(self.framing.target + FEED);
        rest.extend_from_slice(&self.span[lt..]);
        let mut bytes = std::mem::replace(&mut self.span, rest);
        bytes.truncate(lt);
        let entry = self.entry.replace(at);
        (self.seal)(LexBatch::span(bytes, entry, Some(at)))
    }

    /// End of input: the unsealed span is the document's last.
    fn finish(mut self) -> ParseResult<()> {
        let bytes = std::mem::take(&mut self.span);
        (self.seal)(LexBatch::span(bytes, self.entry, None))
    }

    /// Finish the input with the tokenizing reader, from the unsealed
    /// span's entry: its bytes, then the rest of `source`.
    fn fall_back<R: BufRead>(mut self, source: &mut R, path: &Path) -> ParseResult<()> {
        let prefix = self.entry.map_or(&b""[..], Continuation::prefix);
        let mut replay = Vec::with_capacity(prefix.len() + self.span.len());
        replay.extend_from_slice(prefix);
        replay.extend_from_slice(&self.span);
        self.span = Vec::new();
        let halted = &self.halted;
        let input = Halting {
            inner: io::Read::chain(io::Cursor::new(replay), source),
            halted,
        };
        let mut reader = xml_reader(input);
        let state = continue_at(&mut reader, self.entry)?;
        let mut sink = LexSink::new(self.framing.target, &mut self.seal, halted);
        let part = Part {
            first: self.entry.is_none(),
            exit: None,
        };
        let result = drive_osc_from(&mut reader, &mut sink, path, state, part);
        sink.finish_after(result)
    }
}

/// The rest of an input whose next read failed: that failure, then nothing.
struct FailsWith(Option<io::Error>);

impl io::Read for FailsWith {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        self.0.take().map_or(Ok(0), Err)
    }
}

impl BufRead for FailsWith {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        match self.0.take() {
            Some(e) => Err(e),
            None => Ok(&[]),
        }
    }

    fn consume(&mut self, _: usize) {}
}

/// A reader that refuses once `halted` answers true, so a fallback reading
/// a stretch of input that produces no callbacks still stops promptly.
struct Halting<'h, R, H> {
    inner: R,
    halted: &'h H,
}

impl<R: BufRead, H: Fn() -> bool> io::Read for Halting<'_, R, H> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = {
            let available = self.fill_buf()?;
            let n = available.len().min(buf.len());
            buf[..n].copy_from_slice(&available[..n]);
            n
        };
        self.consume(n);
        Ok(n)
    }
}

impl<R: BufRead, H: Fn() -> bool> BufRead for Halting<'_, R, H> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if (self.halted)() {
            return Err(io::Error::from(stopped()));
        }
        self.inner.fill_buf()
    }

    fn consume(&mut self, amount: usize) {
        self.inner.consume(amount);
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Body, DeleteChildren, OscSink, parse_osc_path};
    use super::*;
    use quick_xml::events::BytesStart;

    /// Every callback in order, with its raw tag content.
    #[derive(Default)]
    struct Log(Vec<String>);

    fn raw(e: &BytesStart<'_>) -> String {
        let text: &str = e;
        text.to_owned()
    }

    impl OscSink for Log {
        const DELETE_CHILDREN: DeleteChildren = DeleteChildren::Route;

        fn begin_element(
            &mut self,
            action: OscAction,
            kind: crate::ElementKind,
            e: &BytesStart<'_>,
        ) -> ParseResult<()> {
            // A sink's own refusal, which the worker raises in a batch.
            if raw(e).contains("lat=\"north\"") {
                return Err(Error::invalid_data("unparsable latitude"));
            }
            self.0.push(format!("begin {action:?} {kind:?} {}", raw(e)));
            Ok(())
        }
        fn tag(&mut self, e: &BytesStart<'_>) -> ParseResult<()> {
            self.0.push(format!("tag {}", raw(e)));
            Ok(())
        }
        fn nd(&mut self, e: &BytesStart<'_>) -> ParseResult<()> {
            self.0.push(format!("nd {}", raw(e)));
            Ok(())
        }
        fn member(&mut self, e: &BytesStart<'_>) -> ParseResult<()> {
            self.0.push(format!("member {}", raw(e)));
            Ok(())
        }
        fn end_element(&mut self) -> ParseResult<()> {
            self.0.push("end".to_owned());
            Ok(())
        }
    }

    /// What reading a document gives: its callbacks, or the text of its
    /// first refusal (compared as the user would read it).
    #[derive(Debug, PartialEq)]
    enum Reading {
        Read(Vec<String>),
        Refused(String),
    }

    impl Reading {
        fn of(result: ParseResult<()>, log: Log) -> Self {
            match result {
                Ok(()) => Self::Read(log.0),
                Err(e) => Self::Refused(e.to_string()),
            }
        }

        fn is_refused(&self) -> bool {
            matches!(self, Self::Refused(_))
        }

        fn read(self, what: &str) -> Vec<String> {
            match self {
                Self::Read(log) => log,
                Self::Refused(e) => panic!("{what}: {e}"),
            }
        }
    }

    /// The document at `path` read whole on one thread.
    fn serial(path: &Path) -> Reading {
        let host = crate::host::Host::for_tests();
        let host = host.entered();
        let mut log = Log::default();
        let result = parse_osc_path(path, &mut log, &host.invocation);
        Reading::of(result, log)
    }

    /// The document at `path` framed at `framing`, read `chunk` bytes at a
    /// time, then every batch parsed in order: the first failure in
    /// document order (a batch's, else the reader's) or the callbacks.
    /// Also returns how many batches were spans and how many transcripts.
    fn framed(path: &Path, framing: Framing, chunk: usize) -> (Reading, usize, usize) {
        let mut batches = Vec::new();
        let file = std::fs::File::open(path).expect("open");
        let mut source = BufReader::with_capacity(chunk, file);
        let read = Framer::new(
            framing,
            |batch| {
                batches.push(batch);
                Ok(())
            },
            || false,
        )
        .run(&mut source, path);
        let spans = batches
            .iter()
            .filter(|b| matches!(b.body, Body::Span(_)))
            .count();
        let transcripts = batches.len() - spans;
        let mut log = Log::default();
        for batch in &batches {
            if let Err(e) = batch.replay(&mut log, path) {
                return (Reading::of(Err(e), log), spans, transcripts);
            }
        }
        (Reading::of(read, log), spans, transcripts)
    }

    const HUGE: usize = usize::MAX;

    /// One framed reading: the framing and read size, then what
    /// [`framed`] returned.
    type Framed = ((Framing, usize), (Reading, usize, usize));

    /// The framings every document is read at: a cut at every cut point
    /// (target 1), a few, and none; and a ceiling that falls back almost at
    /// once. Each at a one-byte read (every read boundary), a short odd one
    /// and a large one.
    fn readings(xml: &str) -> (Reading, Vec<Framed>) {
        let scratch = crate::path_guard::tests::scratch();
        let path = scratch.path().join("in.osc");
        std::fs::write(&path, xml).expect("write");
        let whole = serial(&path);
        let mut out = Vec::new();
        for (target, ceiling) in [(1, HUGE), (64, HUGE), (HUGE, HUGE), (1, 64), (64, 96)] {
            for chunk in [1, 7, 4096] {
                let framing = Framing { target, ceiling };
                out.push(((framing, chunk), framed(&path, framing, chunk)));
            }
        }
        (whole, out)
    }

    /// Every framing reads `xml` as the serial reader does.
    fn assert_parity(xml: &str) -> Reading {
        let (whole, framings) = readings(xml);
        for ((framing, chunk), (reading, ..)) in &framings {
            assert_eq!(
                reading, &whole,
                "framing {framing:?}, {chunk}-byte reads, document:\n{xml}"
            );
        }
        whole
    }

    /// Markup a framer must step over exactly: quoted `>` and both quotes
    /// in values, element-looking text in comments, CDATA and a PI, end tags
    /// with whitespace, an ignored element under the root holding a
    /// comment, references in text, an empty section, every action, and
    /// comments and a PI around the root.
    const RICH: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!-- lead <node id="7"/> -->
<osmChange version="0.6" generator="a>b'c">
  <note>keep <!-- <create> --> out</note>
  <create>
    <!-- a comment with <node id="9"/> inside -->
    <node id="1" lat="1" lon="2"><tag k="a" v="x>y"/><tag k='b' v='"q">'/></node >
    <?pi data > more <node/> ?>
    <![CDATA[ <node id="8"/> ]]>
    <way id="2"><nd ref="1"/><nd ref="1"/><tag k="h" v="&amp;&#60;"/></way>
  </create>
  <modify><relation id="3"><member type="way" ref="2" role="o>u"/></relation></modify >
  <delete><node id="4" lat="0" lon="0"><tag k="z" v="1"/></node></delete>
  <create/>
  &amp;
  <create><node id="5" lat="3" lon="4"/></create>
</osmChange >
<!-- tail --> <?tail?>
"#;

    #[test]
    fn every_framing_reads_a_document_as_the_serial_reader_does() {
        let whole = assert_parity(RICH).read("the serial reader accepts the document");
        assert_eq!(whole.iter().filter(|e| e.starts_with("begin")).count(), 5);
    }

    /// The spans and transcripts `xml` was read as at one framing.
    fn paths(xml: &str, target: usize, ceiling: usize, chunk: usize) -> (usize, usize) {
        let (_, framings) = readings(xml);
        framings
            .iter()
            .find(|((f, c), _)| f.target == target && f.ceiling == ceiling && *c == chunk)
            .map(|(_, (_, spans, transcripts))| (*spans, *transcripts))
            .expect("framing read")
    }

    /// A document that holds a long stretch with no cut point after its
    /// first two objects (each under 64 bytes, so a ceiling of 64 lets a
    /// span carry the first before the run forces the fallback).
    fn long_run() -> String {
        let node = |id: u32| format!("<node id=\"{id}\" lat=\"1\" lon=\"2\"/>");
        let refs = "&amp;".repeat(64);
        let note = format!("<note>{}</note>", "<!--x-->".repeat(32));
        format!(
            "<osmChange><create>{}{}{refs}{}</create>{note}</osmChange>",
            node(1),
            node(2),
            node(3)
        )
    }

    #[test]
    fn a_cut_at_every_cut_point_makes_spans_and_a_low_ceiling_falls_back() {
        let (spans, transcripts) = paths(RICH, 1, HUGE, 1);
        assert!(
            spans > 10 && transcripts == 0,
            "{spans} spans, {transcripts}"
        );
        assert_eq!(paths(RICH, HUGE, HUGE, 4096), (1, 0));
        // Cuts before the first and the second object, then past the
        // ceiling inside the run of references: the root and a span holding
        // the first object, then the rest of the input transcribed.
        let (spans, transcripts) = paths(&long_run(), 1, 64, 1);
        assert!(
            spans >= 2 && transcripts >= 1,
            "{spans} spans, {transcripts}"
        );
    }

    /// An object's own fault, then a lexical one in the same unsealed
    /// stretch: the object's is the one a serial parse meets first, on the
    /// fallback (the empty comment) as in a span.
    #[test]
    fn a_fault_in_an_object_is_reported_before_a_later_lexical_fault() {
        let xml =
            r#"<osmChange><!----><create><node id="1" lat="north" lon="2"/></modify></osmChange>"#;
        let refused = assert_parity(xml);
        assert!(
            matches!(&refused, Reading::Refused(e) if e.contains("unparsable latitude")),
            "{refused:?}"
        );
        let in_span = xml.replace("<!---->", "");
        let refused = assert_parity(&in_span);
        assert!(
            matches!(&refused, Reading::Refused(e) if e.contains("unparsable latitude")),
            "{refused:?}"
        );
    }

    /// A read failure after bytes that hold an earlier fault: the earlier
    /// fault is reported, as a serial parse reports it.
    #[test]
    fn a_fault_before_a_read_failure_is_reported_first() {
        let xml = r#"<osmChange><create><node id="1" lat="1" lon="2"></way>"#;
        let source = || {
            io::Read::chain(
                io::Cursor::new(xml.as_bytes().to_vec()),
                FailsWith(Some(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "corrupt gzip member",
                ))),
            )
        };
        let path = Path::new("in.osc.gz");
        let mut log = Log::default();
        let mut reader = xml_reader(source());
        let whole = Reading::of(drive_osc(&mut reader, &mut log, path), log);
        assert!(
            matches!(&whole, Reading::Refused(e) if !e.contains("corrupt gzip member")),
            "{whole:?}"
        );
        for target in [1, HUGE] {
            let framing = Framing {
                target,
                ceiling: HUGE,
            };
            let mut batches = Vec::new();
            let read = Framer::new(
                framing,
                |batch| {
                    batches.push(batch);
                    Ok(())
                },
                || false,
            )
            .run(&mut source(), path);
            let mut log = Log::default();
            let mut reading = None;
            for batch in &batches {
                if let Err(e) = batch.replay(&mut log, path) {
                    reading = Some(Reading::Refused(e.to_string()));
                    break;
                }
            }
            let reading = reading.unwrap_or_else(|| Reading::of(read, log));
            assert_eq!(reading, whole, "target {target}");
        }
    }

    #[test]
    fn markup_the_framer_does_not_follow_falls_back_exactly() {
        let doctype = format!(
            "<!DOCTYPE osmChange SYSTEM \"x>y\">{}",
            RICH.trim_start_matches(r#"<?xml version="1.0" encoding="UTF-8"?>"#)
        );
        assert_parity(&doctype).read("an external DOCTYPE is read");
        let empty_comment = RICH.replace("<create/>", "<create/><!---->");
        assert_parity(&empty_comment).read("an empty comment is read");
        let (_, framings) = readings(&empty_comment);
        assert!(
            framings
                .iter()
                .any(|((f, _), (_, spans, transcripts))| f.target == 1
                    && *spans > 1
                    && *transcripts >= 1),
            "the empty comment falls back after spans"
        );
    }

    #[test]
    fn a_long_run_without_a_cut_point_falls_back() {
        assert_parity(&long_run()).read("references and ignored comments are read");
    }

    #[test]
    fn every_framing_refuses_what_the_serial_reader_refuses() {
        let node = r#"<node id="1" lat="1" lon="2"/>"#;
        for xml in [
            format!(r#"<osmChange><create>{node}<?xml version="1.0"?></create></osmChange>"#),
            format!("<osmChange><create>{node}<!-- a--b --></create></osmChange>"),
            r#"<osmChange><create><node id="1" lat="1" lon="2"></way></create></osmChange>"#
                .to_owned(),
            format!("<osmChange><create>{node}</modify></osmChange>"),
            format!("<osmChange><create>{node}{node}"),
            format!(
                r#"<osmChange><create>{node}<node id="2 lat="1" lon="2"/></create></osmChange>"#
            ),
            format!(
                r#"<osmChange><create>{node}<node id="2" lat="<" lon="2"/></create></osmChange>"#
            ),
            format!("<osmChange><create>{node}</node></create></osmChange>"),
            format!("<osmChange><create>{node}<!x></create></osmChange>"),
            format!("<osmChange><create>{node}</create></osmChange><create/>"),
            format!("<osmChange><create>{node}</create></osmChange>x"),
            format!("<osmChange><create>{node}<create/></create></osmChange>"),
            format!("<osmChange><create>{node}</create><node id=\"2\"/></osmChange>"),
            format!("<osmChange><create>{node}<![CDATA[ x </create></osmChange>"),
            format!("<osmChange><create>{node}<"),
        ] {
            let refused = assert_parity(&xml);
            assert!(refused.is_refused(), "accepted:\n{xml}");
        }
    }

    #[test]
    fn a_regular_file_is_framed_into_spans() {
        let scratch = crate::path_guard::tests::scratch();
        let path = scratch.path().join("in.osc");
        let mut xml = String::from("<osmChange version=\"0.6\"><create>");
        for k in 0..2000 {
            xml.push_str(&format!(
                "<node id=\"{k}\" lat=\"1\" lon=\"2\"><tag k=\"n\" v=\"{k}\"/></node>"
            ));
        }
        xml.push_str("</create></osmChange>");
        std::fs::write(&path, &xml).expect("write");
        let host = crate::host::Host::for_tests();
        let host = host.entered();
        let mut batches = Vec::new();
        let framing = Framing {
            target: 1 << 12,
            ceiling: HUGE,
        };
        frame_osc_path(
            &path,
            framing,
            |batch| {
                batches.push(batch);
                Ok(())
            },
            || false,
            &host.invocation,
        )
        .expect("frame");
        assert!(batches.len() > 10, "{}", batches.len());
        assert!(batches.iter().all(|b| matches!(b.body, Body::Span(_))));
        let mut log = Log::default();
        for batch in &batches {
            batch.replay(&mut log, &path).expect("parse span");
        }
        assert_eq!(Reading::Read(log.0), serial(&path));
    }
}
