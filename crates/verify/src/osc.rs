//! Minimal OSC (`.osc` / `.osc.gz`) reader for verify-side delta analysis.
//!
//! The verifier's checks need to know which element IDs an OSC file
//! creates, modifies or deletes, in the input a command consumed and in
//! the output a tool generated.
//!
//! The parser is deliberately narrow: it understands `<osmChange>`
//! blocks, the `<create>` / `<modify>` / `<delete>` containers, and
//! `<node id="N">` / `<way id="N">` / `<relation id="N">` element
//! starts within them. It does **not** parse element bodies (tags,
//! refs, members, coordinates) - those aren't needed for
//! those checks and pulling in a full XML stack just to discard them
//! would be churn. The lexer still ends every markup form where XML 1.0
//! does, and the strict scan matches every end tag against the element it
//! closes, so nesting is checked here rather than borrowed from the XML
//! parser a caller also runs. [`scan_osc_changes`] also decodes each entry's metadata
//! attributes, the part of a change an OPL reading cannot tell apart
//! (`verify merge-changes` and `verify tags-filter-osc`). It scans
//! incrementally and retains only the current tag, so large Planet or
//! regional diffs do not need to fit in memory.

#![allow(
    dead_code,
    reason = "shared verifier helpers consumed by the remaining family slices"
)]

use std::fs::File;
use std::io::{self, BufRead, Cursor, Read};
use std::path::Path;

use flate2::read::MultiGzDecoder;

use crate::error::Error;

/// The `<create>` / `<modify>` / `<delete>` container an element sits in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OscAction {
    Create,
    Modify,
    Delete,
}

/// The element kind of one OSC entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OscKind {
    Node,
    Way,
    Relation,
}

impl OscKind {
    /// Index into a per-kind `[_; 3]` array: node, way, relation.
    pub fn index(self) -> usize {
        match self {
            Self::Node => 0,
            Self::Way => 1,
            Self::Relation => 2,
        }
    }
}

/// Scan an OSC file from disk, calling `each` once per element entry inside
/// an action container, in file order. Nothing is retained between entries:
/// what a caller keeps is its own choice, so a diff the size of a planet
/// costs only what the caller accumulates. Every entry has been delivered
/// before the document is known to be complete, so a caller must not act on
/// what it saw until this returns `Ok`. Gzip is detected by magic bytes (not the
/// filename), so the reader does not depend on how a producer ties
/// compression to the extension it was given.
///
/// Decompression goes through `MultiGzDecoder`, not `GzDecoder`, because that
/// OSC is a concatenation of independently-gzipped members (pbfhogg splices
/// the create/modify/delete sections). Plain `GzDecoder` stops silently at the
/// first member boundary and would yield a truncated-but-valid-looking OSC.
pub fn scan_osc_file(
    path: &Path,
    mut each: impl FnMut(OscAction, OscKind, i64),
) -> Result<(), Error> {
    scan_osc_file_as(path, Ids::Lenient, |action, kind, id, _| {
        each(action, kind, id);
        Ok(())
    })
}

/// The metadata attributes of one change entry that pbfhogg's OSC writers
/// carry through: `version` parsed as an integer, as pbfhogg parses it, and
/// the other five as their attribute values decoded (entity and character
/// references resolved, literal tabs and line breaks normalised to spaces,
/// as an XML parser normalises an attribute value). `None` is an absent
/// attribute.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OscMeta {
    pub version: Option<i64>,
    pub timestamp: Option<String>,
    pub changeset: Option<String>,
    pub uid: Option<String>,
    pub user: Option<String>,
    pub visible: Option<String>,
}

/// One change entry as [`scan_osc_changes`] reads it: the action of the
/// container it sits in, its kind and ID, and its metadata attributes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OscChange {
    pub action: OscAction,
    pub kind: OscKind,
    pub id: i64,
    pub meta: OscMeta,
}

/// [`scan_osc_file_strict`] that also reads each entry's metadata
/// attributes ([`OscMeta`]). An entry whose attributes cannot be read (an
/// unterminated or unquoted value, an unknown entity, a repeated
/// attribute, a `version` that is not an integer) is an error, so the entry
/// a caller sees always carries what the document says.
pub fn scan_osc_changes(path: &Path, mut each: impl FnMut(OscChange)) -> Result<(), Error> {
    scan_osc_file_as(path, Ids::Strict, |action, kind, id, attrs| {
        let meta = parse_meta(attrs)?;
        each(OscChange {
            action,
            kind,
            id,
            meta,
        });
        Ok(())
    })
}

/// The [`OscMeta`] of an entry's attribute text.
fn parse_meta(attrs: &str) -> Result<OscMeta, String> {
    let mut meta = OscMeta::default();
    let mut seen: Vec<&str> = Vec::new();
    for (name, raw) in parse_attributes(attrs)? {
        if seen.contains(&name) {
            return Err(format!("attribute {name} repeated"));
        }
        seen.push(name);
        let slot = match name {
            "version" => {
                let value = decode_attribute(raw)?;
                meta.version = Some(
                    value
                        .parse()
                        .map_err(|_| format!("version {value:?} is not an integer"))?,
                );
                continue;
            }
            "timestamp" => &mut meta.timestamp,
            "changeset" => &mut meta.changeset,
            "uid" => &mut meta.uid,
            "user" => &mut meta.user,
            "visible" => &mut meta.visible,
            _ => continue,
        };
        *slot = Some(decode_attribute(raw)?);
    }
    Ok(meta)
}

/// Every `name="value"` (or single-quoted) pair of a start tag's attribute
/// text, values raw. Anything else is an error.
fn parse_attributes(attrs: &str) -> Result<Vec<(&str, &str)>, String> {
    let mut out = Vec::new();
    let mut rest = attrs.trim_start();
    while !rest.is_empty() {
        let name_end = rest
            .find(|c: char| c == '=' || c.is_ascii_whitespace())
            .ok_or_else(|| format!("attribute {rest:?} has no value"))?;
        let name = &rest[..name_end];
        rest = rest[name_end..].trim_start();
        rest = rest
            .strip_prefix('=')
            .ok_or_else(|| format!("attribute {name} has no value"))?
            .trim_start();
        let quote = rest
            .chars()
            .next()
            .filter(|&c| c == '"' || c == '\'')
            .ok_or_else(|| format!("attribute {name} value is not quoted"))?;
        rest = &rest[1..];
        let end = rest
            .find(quote)
            .ok_or_else(|| format!("attribute {name} value is unterminated"))?;
        out.push((name, &rest[..end]));
        rest = rest[end + 1..].trim_start();
    }
    Ok(out)
}

/// An attribute value as an XML parser reports it: a literal tab, line feed,
/// carriage return or CR LF pair becomes one space, then the five predefined
/// entities and numeric character references are resolved.
fn decode_attribute(raw: &str) -> Result<String, String> {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        match c {
            '\r' => {
                if chars.peek().is_some_and(|&(_, next)| next == '\n') {
                    chars.next();
                }
                out.push(' ');
            }
            '\n' | '\t' => out.push(' '),
            '&' => {
                let end = raw[at..]
                    .find(';')
                    .ok_or_else(|| format!("unterminated reference in {raw:?}"))?;
                let name = &raw[at + 1..at + end];
                let decoded = match name {
                    "amp" => '&',
                    "lt" => '<',
                    "gt" => '>',
                    "quot" => '"',
                    "apos" => '\'',
                    _ => char_reference(name)
                        .ok_or_else(|| format!("unknown reference &{name}; in {raw:?}"))?,
                };
                out.push(decoded);
                while chars.peek().is_some_and(|&(i, _)| i <= at + end) {
                    chars.next();
                }
            }
            _ => out.push(c),
        }
    }
    Ok(out)
}

/// A numeric character reference body (between `&` and `;`) as XML 1.0's
/// `CharRef` spells it: `#` and decimal digits, or `#x` (lowercase) and hex
/// digits, at least one, any number of leading zeros, naming a `Char`.
/// Anything else is `None`: a sign (`&#x+41;`, which `from_str_radix` alone
/// would take), an empty body, `&#0;` or `&#xFFFE;` - the references Expat
/// refuses, so osmium's reading of the same document refuses them too.
fn char_reference(name: &str) -> Option<char> {
    let (digits, radix) = if let Some(hex) = name.strip_prefix("#x") {
        (hex, 16)
    } else {
        (name.strip_prefix('#')?, 10)
    };
    if digits.is_empty() {
        return None;
    }
    let mut code: u32 = 0;
    for c in digits.chars() {
        // Saturates past the Unicode range, which the Char test then refuses.
        code = code
            .saturating_mul(radix)
            .saturating_add(c.to_digit(radix)?);
    }
    let is_xml_char =
        matches!(code, 0x9 | 0xA | 0xD | 0x20..=0xD7FF | 0xE000..=0xFFFD | 0x1_0000..=0x10_FFFF);
    if is_xml_char {
        char::from_u32(code)
    } else {
        None
    }
}

/// [`scan_osc_file`] for an OSC a tool generated as a comparison: a node,
/// way or relation inside an action container without a parsable `id` is an
/// error rather than skipped, and so is one outside an action container, an
/// action nested in another or inside an entry, an entry inside an entry, and
/// an end tag that closes something not open. Every entry of the document is
/// then accounted for, and a malformed one cannot drop out of a verdict
/// unseen.
pub fn scan_osc_file_strict(
    path: &Path,
    mut each: impl FnMut(OscAction, OscKind, i64),
) -> Result<(), Error> {
    scan_osc_file_as(path, Ids::Strict, |action, kind, id, _| {
        each(action, kind, id);
        Ok(())
    })
}

/// Whether an action entry without a parsable `id` is skipped or refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ids {
    /// Skipped: an input change file is read for the IDs it does carry.
    Lenient,
    /// Refused: a generated comparison must account for every entry.
    Strict,
}

/// The scan behind every file entry point. `each` gets an entry's action,
/// kind, ID and its start tag's attribute text; an `Err` from it stops the
/// scan as a document error.
fn scan_osc_file_as(
    path: &Path,
    ids: Ids,
    each: impl FnMut(OscAction, OscKind, i64, &str) -> Result<(), String>,
) -> Result<(), Error> {
    let (input, compressed) = open_osc(path)?;
    scan_osc_reader(input, ids, each).map_err(|error| match error {
        OscReadError::Read(error) if compressed => {
            Error::Failed(format!("cannot decompress OSC {}: {error}", path.display()))
        }
        OscReadError::Read(error) => {
            Error::Failed(format!("cannot read OSC {}: {error}", path.display()))
        }
        OscReadError::Utf8(error) => Error::Failed(format!(
            "OSC {} is not valid UTF-8: {error}",
            path.display()
        )),
        OscReadError::Parse(why) => Error::Failed(format!(
            "{} is not a complete OSC document: {why}",
            path.display()
        )),
    })
}

/// Open an OSC file as its decompressed byte stream, gzip detected by magic
/// as in [`scan_osc_file`]. The flag says whether it was gzip.
fn open_osc(path: &Path) -> Result<(Box<dyn Read>, bool), Error> {
    let mut file = File::open(path)
        .map_err(|e| Error::Failed(format!("cannot open OSC {}: {e}", path.display())))?;
    let mut magic = [0; 2];
    let mut prefix_len = 0;
    while prefix_len < magic.len() {
        let read = file
            .read(&mut magic[prefix_len..])
            .map_err(|e| Error::Failed(format!("cannot read OSC {}: {e}", path.display())))?;
        if read == 0 {
            break;
        }
        prefix_len += read;
    }
    let input = Cursor::new(magic[..prefix_len].to_vec()).chain(file);
    let compressed = prefix_len == 2 && magic == [0x1f, 0x8b];
    if compressed {
        Ok((Box::new(MultiGzDecoder::new(input)), true))
    } else {
        Ok((Box::new(input), false))
    }
}

/// The offset of the first byte at which two OSC files' decompressed contents
/// differ (where the shorter one ends, if one is a prefix of the other), or
/// `None` when they are identical. The gzip framing is not compared: pbfhogg
/// splices independently-gzipped members, so two runs may frame the same
/// document differently.
pub fn first_content_difference(a: &Path, b: &Path) -> Result<Option<u64>, Error> {
    let (a_input, _) = open_osc(a)?;
    let (b_input, _) = open_osc(b)?;
    first_stream_difference(a_input, b_input).map_err(|e| {
        Error::Failed(format!(
            "cannot read OSC {} or {}: {e}",
            a.display(),
            b.display()
        ))
    })
}

/// The offset at which two byte streams first differ, read through fixed
/// buffers; a length difference counts at the shorter stream's end.
pub(crate) fn first_stream_difference(a: impl Read, b: impl Read) -> io::Result<Option<u64>> {
    let mut a = io::BufReader::new(a);
    let mut b = io::BufReader::new(b);
    let mut offset: u64 = 0;
    loop {
        let a_buf = a.fill_buf()?;
        let b_buf = b.fill_buf()?;
        if a_buf.is_empty() || b_buf.is_empty() {
            return Ok((a_buf.len() != b_buf.len()).then_some(offset));
        }
        let n = a_buf.len().min(b_buf.len());
        if let Some(i) = a_buf[..n].iter().zip(&b_buf[..n]).position(|(x, y)| x != y) {
            return Ok(Some(offset + i as u64));
        }
        a.consume(n);
        b.consume(n);
        offset += n as u64;
    }
}

/// Section the parser is currently inside. `None` means "between
/// `<osmChange>` and the first action tag" or "after closing one
/// action tag and before opening the next."
#[derive(Debug, Clone, Copy, Default)]
enum Section {
    #[default]
    None,
    Create,
    Modify,
    Delete,
}

/// Where the parser stands relative to the `<osmChange>` root element.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Root {
    /// No element seen yet (only the prolog: `<?xml?>`, comments, DOCTYPE).
    #[default]
    Before,
    Open,
    Closed,
}

/// Read errors and document errors remain distinct so the file entry point
/// can name decompression, UTF-8, and XML failures accurately.
#[derive(Debug)]
enum OscReadError {
    Read(io::Error),
    Utf8(std::str::Utf8Error),
    Parse(String),
}

/// Validate UTF-8 in bounded chunks. The trailing bytes of a partial code
/// point are carried into the next chunk; the OSC scanner itself only retains
/// the current tag (or a small comment marker), never the element bodies.
#[derive(Default)]
struct Utf8Validator {
    trailing: Vec<u8>,
}

impl Utf8Validator {
    fn push(&mut self, bytes: &[u8]) -> Result<(), std::str::Utf8Error> {
        let mut joined = std::mem::take(&mut self.trailing);
        joined.extend_from_slice(bytes);
        match std::str::from_utf8(&joined) {
            Ok(_) => Ok(()),
            Err(error) if error.error_len().is_none() => {
                self.trailing
                    .extend_from_slice(&joined[error.valid_up_to()..]);
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    fn finish(self) -> Result<(), std::str::Utf8Error> {
        std::str::from_utf8(&self.trailing).map(|_| ())
    }
}

fn scan_osc_reader<R: Read>(
    mut reader: R,
    ids: Ids,
    each: impl FnMut(OscAction, OscKind, i64, &str) -> Result<(), String>,
) -> Result<(), OscReadError> {
    let mut parser = OscParser::new(ids, each);
    let mut validator = Utf8Validator::default();
    let mut chunk = [0; crate::tunables::OSC_READ_BUFFER];
    loop {
        let read = reader.read(&mut chunk).map_err(OscReadError::Read)?;
        if read == 0 {
            validator.finish().map_err(OscReadError::Utf8)?;
            break;
        }
        validator.push(&chunk[..read]).map_err(OscReadError::Utf8)?;
        parser.feed(&chunk[..read]).map_err(OscReadError::Parse)?;
    }
    parser.finish().map_err(OscReadError::Parse)
}

/// Scan already-decompressed OSC XML text, as [`scan_osc_file`] does. Tolerant of whitespace,
/// XML comments, and self-closing elements (`<node id="1" .../>`).
/// Element-body content (tags / refs / members) is silently skipped.
///
/// The document must be an OSC one: its first element must be the
/// `<osmChange>` root and that root must be closed. Without the check, any
/// non-OSC input - an empty file, an `.osm` document, a diff truncated
/// mid-write - parsed as an empty diff, which `verify_merge` reports as
/// "element-identical PASS".
#[cfg(test)]
pub fn scan_osc_text(text: &str, each: impl FnMut(OscAction, OscKind, i64)) -> Result<(), String> {
    scan_osc_text_as(text, Ids::Lenient, each)
}

#[cfg(test)]
fn scan_osc_text_as(
    text: &str,
    ids: Ids,
    mut each: impl FnMut(OscAction, OscKind, i64),
) -> Result<(), String> {
    let mut parser = OscParser::new(ids, |action, kind, id, _: &str| {
        each(action, kind, id);
        Ok(())
    });
    parser.feed(text.as_bytes())?;
    parser.finish()
}

struct OscParser<F> {
    each: F,
    ids: Ids,
    section: Section,
    /// The entry element open in strict mode (`node`, `way`, `relation`).
    open: Option<&'static str>,
    /// Strict mode: the name of every element open, outermost first. Every
    /// end tag must close the innermost one, so an unknown element left
    /// unclosed or closed out of order is refused by the scan itself, not
    /// only by whichever XML parser a caller also runs.
    stack: Vec<String>,
    root: Root,
    /// Whether the prolog already held a DOCTYPE; XML allows one.
    doctype_seen: bool,
    scan: ScanState,
}

/// The lexer's state between bytes. Every markup form ends where XML 1.0
/// ends it, not at the first `>`: a processing instruction at `?>`, a
/// CDATA section at `]]>`, a DOCTYPE at the first `>` outside its quoted
/// literals. Ending any of them early would read their content as markup,
/// where a `<!--` in a processing instruction once swallowed a real entry.
#[derive(Default)]
enum ScanState {
    #[default]
    Text,
    Probe(Vec<u8>),
    /// A tag's text so far, and the quote of the attribute value it is
    /// inside, if any: XML allows a raw `>` in an attribute value, so only a
    /// `>` outside quotes ends the tag.
    Tag(Vec<u8>, Option<u8>),
    /// A comment, with the run of trailing dashes (at most two) seen.
    Comment(u8),
    /// A processing instruction; the flag says the last byte was `?`.
    Pi(bool),
    /// A CDATA section, with the run of trailing `]` (at most two) seen.
    Cdata(u8),
    /// A DOCTYPE, with the quote of the literal it is inside, if any.
    Doctype(Option<u8>),
}

/// How a tag opens or closes an element.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TagKind {
    Start,
    Empty,
    End,
}

/// XML's whitespace: space, tab, line feed and carriage return only.
fn is_xml_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r')
}

/// A tag's text (between `<` and `>`) as its kind, element name and
/// attribute text. The name ends at XML whitespace and is compared exactly;
/// an end tag carries nothing but whitespace after its name. A name that is
/// empty or holds a character no XML name may (`< node>`, `</ node>`,
/// `<node id="1"//>`) is refused.
fn parse_tag(tag: &str) -> Result<(TagKind, &str, &str), String> {
    let (kind, body) = if let Some(rest) = tag.strip_prefix('/') {
        (TagKind::End, rest)
    } else if let Some(rest) = tag.strip_suffix('/') {
        (TagKind::Empty, rest)
    } else {
        (TagKind::Start, tag)
    };
    let end = body.find(is_xml_space).unwrap_or(body.len());
    let (name, rest) = body.split_at(end);
    if name.is_empty() || name.contains(['/', '<', '>', '=', '"', '\'']) {
        return Err(format!("malformed tag <{tag}>"));
    }
    if kind == TagKind::End && !rest.chars().all(is_xml_space) {
        return Err(format!("malformed end tag <{tag}>"));
    }
    // Outside a quoted value a `/` may only be the empty-element marker,
    // which is already stripped; a value ends at its quote, never at `/`.
    if rest.trim_end_matches(is_xml_space).ends_with('/') {
        return Err(format!("malformed tag <{tag}>"));
    }
    Ok((kind, name, rest.trim_start_matches(is_xml_space)))
}

/// The quote state after `byte` inside a tag: a quote opens a value, the
/// same quote closes it, and anything else leaves the state as it was.
fn next_quote(quote: Option<u8>, byte: u8) -> Option<u8> {
    match (quote, byte) {
        (None, b'"' | b'\'') => Some(byte),
        (Some(open), _) if open == byte => None,
        (state, _) => state,
    }
}

impl<F: FnMut(OscAction, OscKind, i64, &str) -> Result<(), String>> OscParser<F> {
    fn new(ids: Ids, each: F) -> Self {
        Self {
            each,
            ids,
            section: Section::None,
            open: None,
            stack: Vec::new(),
            root: Root::Before,
            doctype_seen: false,
            scan: ScanState::Text,
        }
    }

    fn feed(&mut self, bytes: &[u8]) -> Result<(), String> {
        for &byte in bytes {
            let state = std::mem::take(&mut self.scan);
            self.scan = match state {
                ScanState::Text if byte == b'<' => ScanState::Probe(Vec::new()),
                ScanState::Text => ScanState::Text,
                ScanState::Probe(mut prefix) => {
                    prefix.push(byte);
                    self.probe(prefix)?
                }
                ScanState::Tag(tag, None) if byte == b'>' => {
                    self.process_tag(&tag)?;
                    ScanState::Text
                }
                // XML forbids a raw `<` anywhere in a tag, quoted values
                // included; reading past one would let an attribute value
                // swallow an entry's markup.
                ScanState::Tag(..) if byte == b'<' => return Err("a raw < inside a tag".into()),
                ScanState::Tag(mut tag, quote) => {
                    tag.push(byte);
                    ScanState::Tag(tag, next_quote(quote, byte))
                }
                ScanState::Comment(2) if byte == b'>' => ScanState::Text,
                ScanState::Comment(2) => return Err("-- inside an XML comment".into()),
                ScanState::Comment(dashes) => {
                    ScanState::Comment(if byte == b'-' { dashes + 1 } else { 0 })
                }
                ScanState::Pi(true) if byte == b'>' => ScanState::Text,
                ScanState::Pi(_) => ScanState::Pi(byte == b'?'),
                ScanState::Cdata(2) if byte == b'>' => ScanState::Text,
                ScanState::Cdata(brackets) => ScanState::Cdata(if byte == b']' {
                    (brackets + 1).min(2)
                } else {
                    0
                }),
                ScanState::Doctype(None) if byte == b'[' => {
                    return Err("a DOCTYPE internal subset is not supported".into());
                }
                ScanState::Doctype(None) if byte == b'>' => ScanState::Text,
                ScanState::Doctype(quote) => ScanState::Doctype(next_quote(quote, byte)),
            };
        }
        Ok(())
    }

    /// The state after the bytes following a `<` so far: a markup form once
    /// its opening is complete, a tag once it cannot be one, still a probe
    /// while it may yet become one.
    fn probe(&mut self, prefix: Vec<u8>) -> Result<ScanState, String> {
        const COMMENT: &[u8] = b"!--";
        const CDATA: &[u8] = b"![CDATA[";
        const DOCTYPE: &[u8] = b"!DOCTYPE";
        let p = prefix.as_slice();
        if p == b"?" {
            return Ok(ScanState::Pi(false));
        }
        if p == COMMENT {
            return Ok(ScanState::Comment(0));
        }
        if p == CDATA {
            if self.root != Root::Open {
                return Err("a CDATA section outside the root element".into());
            }
            return Ok(ScanState::Cdata(0));
        }
        if p == DOCTYPE {
            if self.root != Root::Before || self.doctype_seen {
                return Err("a DOCTYPE outside the prolog, or a second one".into());
            }
            self.doctype_seen = true;
            return Ok(ScanState::Doctype(None));
        }
        if p.starts_with(b"!") {
            if [COMMENT, CDATA, DOCTYPE].iter().any(|m| m.starts_with(p)) {
                return Ok(ScanState::Probe(prefix));
            }
            return Err(format!(
                "unsupported markup <{}",
                String::from_utf8_lossy(p)
            ));
        }
        match p.last() {
            Some(b'<') => Err("a raw < inside a tag".into()),
            Some(b'>') => {
                self.process_tag(&p[..p.len() - 1])?;
                Ok(ScanState::Text)
            }
            byte => {
                let quote = byte.and_then(|&b| next_quote(None, b));
                Ok(ScanState::Tag(prefix, quote))
            }
        }
    }

    fn process_tag(&mut self, tag: &[u8]) -> Result<(), String> {
        let tag = std::str::from_utf8(tag).map_err(|error| error.to_string())?;
        let (kind, name, attrs) = parse_tag(tag)?;
        if self.ids == Ids::Strict {
            self.check_structure(kind, name)?;
        }
        if kind == TagKind::End {
            match name {
                "create" | "modify" | "delete" => self.section = Section::None,
                // Strict: only the end tag that empties the stack closes the
                // root, never an inner element of the same name.
                "osmChange"
                    if self.root == Root::Open
                        && (self.ids == Ids::Lenient || self.stack.is_empty()) =>
                {
                    self.root = Root::Closed;
                }
                _ => {}
            }
            return Ok(());
        }

        let self_closing = kind == TagKind::Empty;
        match self.root {
            Root::Before if name == "osmChange" => {
                self.root = if self_closing {
                    Root::Closed
                } else {
                    Root::Open
                };
                return Ok(());
            }
            Root::Before => return Err(format!("root element is <{name}>, not <osmChange>")),
            Root::Closed => return Err(format!("element <{name}> after the closing </osmChange>")),
            Root::Open => {}
        }
        match name {
            // A self-closing action block is empty: it opens nothing.
            "create" | "modify" | "delete" if self_closing => {}
            "create" => self.section = Section::Create,
            "modify" => self.section = Section::Modify,
            "delete" => self.section = Section::Delete,
            "node" | "way" | "relation" => {
                match (extract_id(attrs), self.section.action(), kind_of(name)) {
                    (Some(id), Some(action), Some(kind)) => (self.each)(action, kind, id, attrs)
                        .map_err(|why| format!("<{name} id=\"{id}\">: {why}"))?,
                    (None, Some(_), _) if self.ids == Ids::Strict => {
                        return Err(format!("<{name}> in an action block without a parsable id"));
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Strict mode's structure: every element, known or not, is closed by
    /// its own end tag in order, so the root closes with nothing left open;
    /// an action block holds element entries and is not nested; an entry
    /// sits in an action block and holds no entry. Anything else could carry
    /// an entry the scan never delivers, so it is an error. Runs before the
    /// tag is otherwise processed; the root checks are
    /// [`process_tag`](Self::process_tag)'s own.
    ///
    /// What the scan guarantees on its own is element nesting and an
    /// account of every literal entry. It is not a full XML well-formedness
    /// check (attribute syntax outside entries, name characters, references
    /// in text go unread): every caller also has osmium read the same
    /// document, and that reading's success is the rest of the XML check.
    fn check_structure(&mut self, kind: TagKind, name: &str) -> Result<(), String> {
        if kind == TagKind::End {
            match self.stack.last() {
                None => return Err(format!("</{name}> closes nothing open")),
                Some(top) if top != name => {
                    return Err(format!("</{name}> does not close the open <{top}>"));
                }
                Some(_) => {
                    self.stack.pop();
                }
            }
            if kind_of(name).is_some() {
                self.open = None;
            }
            return Ok(());
        }
        let self_closing = kind == TagKind::Empty;
        match name {
            "create" | "modify" | "delete" => {
                if self.section.action().is_some() || self.open.is_some() {
                    return Err(format!("<{name}> inside another action block or entry"));
                }
            }
            "node" | "way" | "relation" => {
                if self.section.action().is_none() {
                    return Err(format!("<{name}> outside an action block"));
                }
                if let Some(open) = self.open {
                    return Err(format!("<{name}> inside an open <{open}>"));
                }
                if !self_closing {
                    self.open = match name {
                        "node" => Some("node"),
                        "way" => Some("way"),
                        _ => Some("relation"),
                    };
                }
            }
            _ => {}
        }
        if !self_closing {
            self.stack.push(name.to_owned());
        }
        Ok(())
    }

    fn finish(self) -> Result<(), String> {
        match self.scan {
            ScanState::Text => {}
            ScanState::Comment(_) => return Err("unterminated XML comment".to_owned()),
            ScanState::Pi(_) => return Err("unterminated processing instruction".to_owned()),
            ScanState::Cdata(_) => return Err("unterminated CDATA section".to_owned()),
            ScanState::Doctype(_) => return Err("unterminated DOCTYPE".to_owned()),
            ScanState::Probe(_) | ScanState::Tag(..) => {
                return Err("incomplete XML tag".to_owned());
            }
        }
        match self.root {
            Root::Closed => Ok(()),
            Root::Before => Err("no <osmChange> root element".to_owned()),
            Root::Open => Err("missing closing </osmChange> (truncated?)".to_owned()),
        }
    }
}

/// Extract the value of the `id` attribute from a tag's attribute
/// string, references resolved. Returns `None` if absent or unparseable. Tolerant of
/// single- and double-quoted values; doesn't validate other attribute
/// shapes.
fn extract_id(attrs: &str) -> Option<i64> {
    // Walk through attributes looking for `id=`. Simple linear scan -
    // attributes can appear in any order and we don't care about the
    // others.
    let bytes = attrs.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Skip whitespace.
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            return None;
        }
        // Read attribute name.
        let name_start = i;
        while i < bytes.len() && bytes[i] != b'=' && !bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let name = &attrs[name_start..i];
        // Skip whitespace before `=`.
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] != b'=' {
            return None;
        }
        i += 1; // skip '='
        // Skip whitespace after `=`.
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            return None;
        }
        let quote = bytes[i];
        if quote != b'"' && quote != b'\'' {
            return None;
        }
        i += 1;
        let val_start = i;
        while i < bytes.len() && bytes[i] != quote {
            i += 1;
        }
        if i >= bytes.len() {
            return None;
        }
        let val = &attrs[val_start..i];
        i += 1; // skip closing quote
        if name == "id" {
            // Decoded as `parse_meta` decodes the other attributes, so a
            // legal `id="&#49;"` reads as 1 rather than as unparsable.
            return decode_attribute(val).ok()?.parse().ok();
        }
    }
    None
}

fn kind_of(name: &str) -> Option<OscKind> {
    match name {
        "node" => Some(OscKind::Node),
        "way" => Some(OscKind::Way),
        "relation" => Some(OscKind::Relation),
        _ => None,
    }
}

impl Section {
    fn action(self) -> Option<OscAction> {
        match self {
            Self::None => None,
            Self::Create => Some(OscAction::Create),
            Self::Modify => Some(OscAction::Modify),
            Self::Delete => Some(OscAction::Delete),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use std::collections::BTreeSet;

    /// Every entry a scan delivered, by action and kind.
    #[derive(Debug, Default)]
    struct Seen {
        created_nodes: BTreeSet<i64>,
        created_ways: BTreeSet<i64>,
        created_relations: BTreeSet<i64>,
        modified_nodes: BTreeSet<i64>,
        deleted_nodes: BTreeSet<i64>,
        deleted_ways: BTreeSet<i64>,
        deleted_relations: BTreeSet<i64>,
        other: usize,
    }

    impl Seen {
        fn record(&mut self, action: OscAction, kind: OscKind, id: i64) {
            let set = match (action, kind) {
                (OscAction::Create, OscKind::Node) => &mut self.created_nodes,
                (OscAction::Create, OscKind::Way) => &mut self.created_ways,
                (OscAction::Create, OscKind::Relation) => &mut self.created_relations,
                (OscAction::Modify, OscKind::Node) => &mut self.modified_nodes,
                (OscAction::Delete, OscKind::Node) => &mut self.deleted_nodes,
                (OscAction::Delete, OscKind::Way) => &mut self.deleted_ways,
                (OscAction::Delete, OscKind::Relation) => &mut self.deleted_relations,
                _ => {
                    self.other += 1;
                    return;
                }
            };
            set.insert(id);
        }

        fn is_empty(&self) -> bool {
            self.created_nodes.is_empty()
                && self.created_ways.is_empty()
                && self.created_relations.is_empty()
                && self.modified_nodes.is_empty()
                && self.deleted_nodes.is_empty()
                && self.deleted_ways.is_empty()
                && self.deleted_relations.is_empty()
                && self.other == 0
        }
    }

    fn parse_osc_text(text: &str) -> Result<Seen, String> {
        let mut seen = Seen::default();
        scan_osc_text(text, |a, k, id| seen.record(a, k, id))?;
        Ok(seen)
    }

    fn parse_osc_file(path: &Path) -> Result<Seen, Error> {
        let mut seen = Seen::default();
        scan_osc_file(path, |a, k, id| seen.record(a, k, id))?;
        Ok(seen)
    }

    fn parse_osc_reader(input: impl Read) -> Result<Seen, OscReadError> {
        let mut seen = Seen::default();
        scan_osc_reader(input, Ids::Lenient, |a, k, id, _| {
            seen.record(a, k, id);
            Ok(())
        })?;
        Ok(seen)
    }

    #[test]
    fn entries_arrive_in_file_order_with_duplicates_kept() {
        let mut seen = Vec::new();
        scan_osc_text(
            r#"<osmChange><delete><way id="5"/><node id="3"/><node id="3"/></delete><modify><relation id="1"/></modify></osmChange>"#,
            |a, k, id| seen.push((a, k, id)),
        )
        .unwrap();
        assert_eq!(
            seen,
            vec![
                (OscAction::Delete, OscKind::Way, 5),
                (OscAction::Delete, OscKind::Node, 3),
                (OscAction::Delete, OscKind::Node, 3),
                (OscAction::Modify, OscKind::Relation, 1),
            ]
        );
    }

    const SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6" generator="test">
  <create>
    <node id="100" version="1" lat="55.0" lon="12.0"/>
    <way id="200" version="1">
      <nd ref="1"/>
      <nd ref="2"/>
    </way>
  </create>
  <modify>
    <node id="300" version="2" lat="55.1" lon="12.1"/>
  </modify>
  <delete>
    <node id="400" version="3"/>
    <way id="500" version="2"/>
    <relation id="600" version="1"/>
  </delete>
</osmChange>
"#;

    #[test]
    fn parses_basic_sections() {
        let diff = parse_osc_text(SAMPLE).unwrap();
        assert_eq!(
            diff.created_nodes.iter().copied().collect::<Vec<_>>(),
            vec![100]
        );
        assert_eq!(
            diff.created_ways.iter().copied().collect::<Vec<_>>(),
            vec![200]
        );
        assert!(diff.created_relations.is_empty());
        assert_eq!(
            diff.modified_nodes.iter().copied().collect::<Vec<_>>(),
            vec![300]
        );
        assert_eq!(
            diff.deleted_nodes.iter().copied().collect::<Vec<_>>(),
            vec![400]
        );
        assert_eq!(
            diff.deleted_ways.iter().copied().collect::<Vec<_>>(),
            vec![500]
        );
        assert_eq!(
            diff.deleted_relations.iter().copied().collect::<Vec<_>>(),
            vec![600]
        );
    }

    #[test]
    fn decodes_multi_member_gzip_osc() {
        use flate2::Compression;
        use flate2::write::GzEncoder;
        use std::io::Write;

        // pbfhogg's `diff --format osc` splices independently-gzipped
        // fragments; emulate a two-member stream. A plain GzDecoder would stop
        // after the first member - MultiGzDecoder must return both.
        let gz = |s: &str| {
            let mut e = GzEncoder::new(Vec::new(), Compression::default());
            e.write_all(s.as_bytes()).unwrap();
            e.finish().unwrap()
        };
        let mut multi = gz(r#"<osmChange><create><node id="1" version="1"/></create>"#);
        multi.extend(gz(
            r#"<delete><way id="2" version="3"/></delete></osmChange>"#,
        ));
        assert_eq!(&multi[..2], &[0x1f, 0x8b], "gzip magic");

        // Detected as gzip by magic even with a bare `.osc` name.
        let scratch_path = pbfhogg_test_scratch::scratch();
        let path = scratch_path.path().join("fixture");
        std::fs::write(&path, multi).unwrap();
        let diff = parse_osc_file(&path).unwrap();
        assert_eq!(
            diff.created_nodes.iter().copied().collect::<Vec<_>>(),
            vec![1]
        );
        assert_eq!(
            diff.deleted_ways.iter().copied().collect::<Vec<_>>(),
            vec![2]
        );
    }

    #[test]
    fn content_difference_ignores_gzip_framing() {
        use flate2::Compression;
        use flate2::write::GzEncoder;
        use std::io::Write;

        let gz = |s: &str| {
            let mut e = GzEncoder::new(Vec::new(), Compression::default());
            e.write_all(s.as_bytes()).unwrap();
            e.finish().unwrap()
        };
        let doc = r#"<osmChange><create><node id="1" version="1"/></create></osmChange>"#;
        let (head, tail) = doc.split_at(30);
        let mut two_members = gz(head);
        two_members.extend(gz(tail));

        let scratch_one = pbfhogg_test_scratch::scratch();
        let one = scratch_one.path().join("fixture");
        let scratch_two = pbfhogg_test_scratch::scratch();
        let two = scratch_two.path().join("fixture");
        let scratch_plain = pbfhogg_test_scratch::scratch();
        let plain = scratch_plain.path().join("fixture");
        std::fs::write(&one, gz(doc)).unwrap();
        std::fs::write(&two, two_members).unwrap();
        std::fs::write(&plain, doc).unwrap();
        assert_eq!(first_content_difference(&one, &two).unwrap(), None);
        assert_eq!(first_content_difference(&one, &plain).unwrap(), None);
    }

    #[test]
    fn content_difference_names_the_first_differing_offset() {
        let at = |a: &str, b: &str| first_stream_difference(a.as_bytes(), b.as_bytes()).unwrap();
        assert_eq!(at("abc", "abc"), None);
        assert_eq!(at("abc", "abd"), Some(2));
        assert_eq!(at("abc", "abcd"), Some(3));
        assert_eq!(at("abcd", "abc"), Some(3));
        assert_eq!(at("", ""), None);
    }

    #[test]
    fn plain_text_osc_passes_through() {
        let diff = parse_osc_reader(Cursor::new(SAMPLE.as_bytes())).unwrap();
        assert!(diff.created_nodes.contains(&100));
        assert!(diff.modified_nodes.contains(&300));
        assert!(diff.deleted_relations.contains(&600));
    }

    #[test]
    fn handles_self_closing_elements() {
        let xml = r#"<osmChange><delete><node id="1" version="1"/><way id="2" version="1"/></delete></osmChange>"#;
        let diff = parse_osc_text(xml).unwrap();
        assert_eq!(
            diff.deleted_nodes.iter().copied().collect::<Vec<_>>(),
            vec![1]
        );
        assert_eq!(
            diff.deleted_ways.iter().copied().collect::<Vec<_>>(),
            vec![2]
        );
    }

    #[test]
    fn handles_explicit_close_tags() {
        let xml = r#"<osmChange><create>
  <way id="42" version="1"><nd ref="100"/><nd ref="200"/></way>
</create></osmChange>"#;
        let diff = parse_osc_text(xml).unwrap();
        assert_eq!(
            diff.created_ways.iter().copied().collect::<Vec<_>>(),
            vec![42]
        );
    }

    #[test]
    fn handles_multiple_blocks_of_same_kind() {
        // OSC tools sometimes emit multiple <delete>...</delete> blocks
        // for the same diff (one per object kind, etc).
        let xml = r#"<osmChange>
<delete><node id="1" version="1"/></delete>
<delete><way id="2" version="1"/></delete>
</osmChange>"#;
        let diff = parse_osc_text(xml).unwrap();
        assert_eq!(
            diff.deleted_nodes.iter().copied().collect::<Vec<_>>(),
            vec![1]
        );
        assert_eq!(
            diff.deleted_ways.iter().copied().collect::<Vec<_>>(),
            vec![2]
        );
    }

    #[test]
    fn ignores_xml_comments() {
        let xml = r#"<osmChange>
<!-- ignored: <node id="999"/> -->
<delete><node id="1" version="1"/></delete>
</osmChange>"#;
        let diff = parse_osc_text(xml).unwrap();
        assert_eq!(
            diff.deleted_nodes.iter().copied().collect::<Vec<_>>(),
            vec![1]
        );
    }

    #[test]
    fn ignores_processing_instructions_and_doctype() {
        let xml = r#"<?xml version="1.0"?>
<!DOCTYPE osmChange>
<osmChange><delete><node id="7" version="1"/></delete></osmChange>"#;
        let diff = parse_osc_text(xml).unwrap();
        assert_eq!(
            diff.deleted_nodes.iter().copied().collect::<Vec<_>>(),
            vec![7]
        );
    }

    #[test]
    fn handles_negative_ids() {
        // Negative IDs are valid in OSM (placeholder IDs for new
        // objects in JOSM-style edits).
        let xml = r#"<osmChange><create><node id="-12345" version="1" lat="0" lon="0"/></create></osmChange>"#;
        let diff = parse_osc_text(xml).unwrap();
        assert!(diff.created_nodes.contains(&-12345));
    }

    #[test]
    fn handles_single_quoted_id() {
        let xml = "<osmChange><delete><node id='1' version='1'/></delete></osmChange>";
        let diff = parse_osc_text(xml).unwrap();
        assert_eq!(
            diff.deleted_nodes.iter().copied().collect::<Vec<_>>(),
            vec![1]
        );
    }

    #[test]
    fn ignores_elements_outside_action_blocks() {
        // Some OSC variants put the elements directly under <osmChange>;
        // those aren't real OSC and we ignore them.
        let xml = r#"<osmChange>
<node id="1" version="1"/>
<delete><node id="2" version="1"/></delete>
</osmChange>"#;
        let diff = parse_osc_text(xml).unwrap();
        assert!(diff.created_nodes.is_empty());
        assert!(diff.modified_nodes.is_empty());
        assert_eq!(
            diff.deleted_nodes.iter().copied().collect::<Vec<_>>(),
            vec![2]
        );
    }

    #[test]
    fn a_raw_gt_inside_an_attribute_value_does_not_end_the_tag() {
        // XML allows `>` unescaped in an attribute value; ending the tag
        // there would read `<node user="` with no id and lose the entry.
        let xml = r#"<osmChange><modify><node user=">" id="1" version="7"/><way user='a>b' id="2"><tag k="x" v="1>0"/></way></modify></osmChange>"#;
        for ids in [Ids::Lenient, Ids::Strict] {
            let mut seen = Vec::new();
            scan_osc_text_as(xml, ids, |a, k, id| seen.push((a, k, id))).unwrap();
            assert_eq!(
                seen,
                vec![
                    (OscAction::Modify, OscKind::Node, 1),
                    (OscAction::Modify, OscKind::Way, 2)
                ],
                "{ids:?}"
            );
        }
        assert!(parse_osc_text(r#"<osmChange><create><node id="1" user="a>"#).is_err());
    }

    #[test]
    fn missing_id_attribute_is_skipped() {
        // Defensive: a malformed element with no id is skipped, not a panic.
        let xml = r#"<osmChange><delete><node version="1"/></delete></osmChange>"#;
        let diff = parse_osc_text(xml).unwrap();
        assert!(diff.is_empty());
    }

    #[test]
    fn strict_ids_refuse_an_action_entry_without_a_parsable_id() {
        // A generated comparison that loses an entry to a bad id could pass
        // with the rest of its entries still agreeing with its exit status.
        for xml in [
            r#"<osmChange><modify><relation id="x"><member type="node" ref="1" role=""/></relation></modify><create><node id="2"/></create></osmChange>"#,
            r#"<osmChange><delete><way version="1"/></delete></osmChange>"#,
        ] {
            let err = scan_osc_text_as(xml, Ids::Strict, |_, _, _| {}).unwrap_err();
            assert!(err.contains("without a parsable id"), "{err}");
        }
    }

    #[test]
    fn strict_structure_refuses_an_entry_the_scan_could_not_deliver() {
        // Each of these hides an entry from a scan that only reads entries
        // inside one open action block.
        for (xml, why) in [
            (
                r#"<osmChange><create><node id="2"/></create><relation id="7"/></osmChange>"#,
                "outside an action block",
            ),
            (
                r#"<osmChange><create><modify><node id="2"/></modify></create></osmChange>"#,
                "inside another action block",
            ),
            (
                r#"<osmChange><create><relation id="1"><node id="2"/></relation></create></osmChange>"#,
                "inside an open <relation>",
            ),
            (
                r#"<osmChange><create><way id="1"></relation></create></osmChange>"#,
                "does not close the open <way>",
            ),
            (
                r#"<osmChange><create><way id="1"></create></osmChange>"#,
                "does not close the open <way>",
            ),
            (
                r#"<osmChange></create></osmChange>"#,
                "does not close the open <osmChange>",
            ),
            (
                r#"<osmChange><create><node id="2"/></delete></osmChange>"#,
                "does not close the open <create>",
            ),
            (
                r#"<osmChange><create><node id="1"/></osmChange>"#,
                "does not close the open <create>",
            ),
        ] {
            let err = scan_osc_text_as(xml, Ids::Strict, |_, _, _| {}).unwrap_err();
            assert!(err.contains(why), "{xml}: {err}");
            // The lenient reader keeps reading what it can.
            assert!(scan_osc_text(xml, |_, _, _| {}).is_ok(), "{xml}");
        }
        // A well-formed document, with bodies and an empty action block.
        let mut seen = Vec::new();
        scan_osc_text_as(
            r#"<osmChange><modify/><create><relation id="1"><member type="node" ref="2" role=""/><tag k="a" v="b"/></relation><node id="3"/></create><delete><way id="4"/></delete></osmChange>"#,
            Ids::Strict,
            |a, k, id| seen.push((a, k, id)),
        )
        .unwrap();
        assert_eq!(
            seen,
            vec![
                (OscAction::Create, OscKind::Relation, 1),
                (OscAction::Create, OscKind::Node, 3),
                (OscAction::Delete, OscKind::Way, 4),
            ]
        );
    }

    #[test]
    fn empty_osmchange_yields_empty_diff() {
        assert!(
            parse_osc_text("<osmChange></osmChange>")
                .unwrap()
                .is_empty()
        );
        assert!(
            parse_osc_text(r#"<?xml version="1.0"?><osmChange version="0.6"/>"#)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn non_osc_input_is_refused_not_an_empty_diff() {
        // The bug: each of these parsed as an empty diff, which verify_merge
        // reported as "element-identical PASS".
        let empty = parse_osc_text("").unwrap_err();
        assert!(empty.contains("no <osmChange>"), "got: {empty}");
        assert!(parse_osc_text("   \n").is_err());
        assert!(parse_osc_text("not xml at all").is_err());
        let osm = parse_osc_text(r#"<osm version="0.6"><node id="1"/></osm>"#).unwrap_err();
        assert!(osm.contains("<osm>"), "got: {osm}");
    }

    #[test]
    fn truncated_osc_is_refused() {
        let err = parse_osc_text(r#"<osmChange><delete><node id="1" version="1"/>"#).unwrap_err();
        assert!(err.contains("</osmChange>"), "got: {err}");
        // Cut off inside a tag.
        assert!(parse_osc_text(r#"<osmChange><delete><node id="1" vers"#).is_err());
    }

    #[test]
    fn content_after_root_is_refused() {
        let err = parse_osc_text(
            r#"<osmChange></osmChange><osmChange><delete><node id="1"/></delete></osmChange>"#,
        )
        .unwrap_err();
        assert!(err.contains("after the closing"), "got: {err}");
    }

    #[test]
    fn parse_osc_file_names_the_file_on_refusal() {
        let scratch_path = pbfhogg_test_scratch::scratch();
        let path = scratch_path.path().join("fixture");
        std::fs::write(&path, b"").unwrap();
        let err = parse_osc_file(&path).unwrap_err().to_string();
        assert!(err.contains(&path.display().to_string()), "got: {err}");
        assert!(err.contains("not a complete OSC document"), "got: {err}");
    }

    #[test]
    fn extract_id_handles_attrs_in_any_order() {
        assert_eq!(extract_id(r#"version="1" id="42""#), Some(42));
        assert_eq!(extract_id(r#"id="42" version="1""#), Some(42));
        assert_eq!(extract_id(r#"  id =  "42"  "#), Some(42));
    }

    #[test]
    fn extract_id_resolves_character_references() {
        assert_eq!(extract_id(r#"id="&#49;&#x32;""#), Some(12));
        assert_eq!(extract_id(r#"id="&bogus;""#), None);
    }

    #[test]
    fn metadata_is_decoded_with_absence_kept_apart_from_empty() {
        let meta = parse_meta(
            "id=\"1\" version=\"07\" user='a &amp; b&#10;c\td' uid=\"\" timestamp=\"t\" \
             visible=\"false\" lat=\"1\"",
        )
        .unwrap();
        assert_eq!(meta.version, Some(7));
        assert_eq!(meta.user.as_deref(), Some("a & b\nc d"));
        assert_eq!(meta.uid.as_deref(), Some(""));
        assert_eq!(meta.timestamp.as_deref(), Some("t"));
        assert_eq!(meta.visible.as_deref(), Some("false"));
        assert_eq!(meta.changeset, None);
        assert_eq!(decode_attribute("a\r\nb\rc&#x41;&lt;").unwrap(), "a b cA<");
    }

    #[test]
    fn unreadable_metadata_is_refused() {
        for attrs in [
            "id=\"1\" version=\"x\"",
            "id=\"1\" user=\"a",
            "id=\"1\" user=a",
            "id=\"1\" user=\"&nope;\"",
            "id=\"1\" user=\"a\" user=\"b\"",
            "id=\"1\" user",
        ] {
            assert!(parse_meta(attrs).is_err(), "{attrs}");
        }
    }

    #[test]
    fn character_references_follow_the_xml_charref_spelling() {
        for (raw, want) in [
            ("&#x41;", "A"),
            ("&#x0041;", "A"),
            ("&#x6a;", "j"),
            ("&#065;", "A"),
            ("&#xD;&#9;&#10;", "\r\t\n"),
            ("&#x10FFFF;", "\u{10FFFF}"),
        ] {
            assert_eq!(decode_attribute(raw).unwrap(), want, "{raw}");
        }
        for raw in [
            "&#x+41;",
            "&#+65;",
            "&#-65;",
            "&#x;",
            "&#;",
            "&#X41;",
            "&#x 41;",
            "&#0;",
            "&#x1F;",
            "&#xFFFE;",
            "&#xD800;",
            "&#x110000;",
            "&#99999999999999999999;",
        ] {
            assert!(decode_attribute(raw).is_err(), "{raw}");
        }
    }

    #[test]
    fn the_change_scan_names_the_entry_whose_metadata_it_refuses() {
        let scratch_path = pbfhogg_test_scratch::scratch();
        let path = scratch_path.path().join("fixture");
        std::fs::write(
            &path,
            r#"<osmChange><modify><node id="4" version="v"/></modify></osmChange>"#,
        )
        .unwrap();
        let err = scan_osc_changes(&path, |_| {}).unwrap_err().to_string();
        assert!(
            err.contains("id=\"4\"") && err.contains("not an integer"),
            "{err}"
        );

        let scratch_path = pbfhogg_test_scratch::scratch();
        let path = scratch_path.path().join("fixture");
        std::fs::write(
            &path,
            r#"<osmChange><delete><way id="9" version="3" user="u"><nd ref="1"/></way></delete></osmChange>"#,
        )
        .unwrap();
        let mut seen = Vec::new();
        scan_osc_changes(&path, |c| seen.push(c)).unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].action, OscAction::Delete);
        assert_eq!(seen[0].kind, OscKind::Way);
        assert_eq!(seen[0].meta.version, Some(3));
        assert_eq!(seen[0].meta.user.as_deref(), Some("u"));
    }

    #[test]
    fn parse_tag_separates_kind_name_and_attrs() {
        assert_eq!(
            parse_tag(r#"node id="1""#).unwrap(),
            (TagKind::Start, "node", r#"id="1""#)
        );
        assert_eq!(parse_tag("create").unwrap(), (TagKind::Start, "create", ""));
        assert_eq!(
            parse_tag("node\tid=\"1\"/").unwrap(),
            (TagKind::Empty, "node", "id=\"1\"")
        );
        assert_eq!(parse_tag("/way \n").unwrap(), (TagKind::End, "way", ""));
        for bad in [" node", "/ node", "/way x", "node id=\"1\"//", "", "/"] {
            assert!(parse_tag(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn strict_nesting_refuses_unknown_elements_closed_out_of_order() {
        for (xml, why) in [
            (
                r#"<osmChange><create><node id="1"><tag k="a" v="b"></node></create></osmChange>"#,
                "does not close the open <tag>",
            ),
            (
                r#"<osmChange><create><node id="1"/></foo></create></osmChange>"#,
                "does not close the open <create>",
            ),
            (
                r#"<osmChange><create><foo><node id="1"/></create></osmChange>"#,
                "does not close the open <foo>",
            ),
            (r#"<osmChange><osmChange></osmChange>"#, "missing closing"),
            (
                r#"<osmChange><create><node id="1"><tag k="a" v="b"><![CDATA[> </tag>]]></node></create></osmChange>"#,
                "does not close the open <tag>",
            ),
        ] {
            let err = scan_osc_text_as(xml, Ids::Strict, |_, _, _| {}).unwrap_err();
            assert!(err.contains(why), "{xml}: {err}");
        }
    }

    #[test]
    fn markup_forms_end_where_xml_ends_them() {
        let strict = |xml: &str| {
            let mut seen = Vec::new();
            scan_osc_text_as(xml, Ids::Strict, |_, _, id| seen.push(id)).map(|()| seen)
        };
        // A processing instruction ends at `?>`: its `<!--` opens no comment
        // that could swallow the node.
        assert_eq!(
            strict(
                "<osmChange><create><?p > <!-- ?><node id=\"1\"/><?p --> ?></create></osmChange>"
            ),
            Ok(vec![1])
        );
        // CDATA content is text, `>` and end-tag lookalikes included.
        assert_eq!(
            strict(
                "<osmChange><create><node id=\"1\"><![CDATA[> </foo>]]]></node></create></osmChange>"
            ),
            Ok(vec![1])
        );
        // A DOCTYPE's quoted literal may hold `>`.
        assert_eq!(
            strict(
                "<!DOCTYPE osmChange SYSTEM \"> <foo>\"><osmChange><create><node id=\"2\"/></create></osmChange>"
            ),
            Ok(vec![2])
        );
        for (xml, why) in [
            (
                "<!DOCTYPE osmChange [<!ENTITY e \"x\">]><osmChange/>",
                "internal subset",
            ),
            (
                "<osmChange><!DOCTYPE osmChange></osmChange>",
                "outside the prolog",
            ),
            ("<!DOCTYPE a><!DOCTYPE a><osmChange/>", "a second one"),
            ("<![CDATA[x]]><osmChange/>", "outside the root"),
            ("<osmChange><!ELEMENT x></osmChange>", "unsupported markup"),
            ("<osmChange><!-- a -- b --></osmChange>", "-- inside"),
            (
                "<osmChange><create marker=\"<node id='1'/>\"></create></osmChange>",
                "raw <",
            ),
            (
                "<osmChange><create><?p</create></osmChange>",
                "unterminated processing",
            ),
        ] {
            let err = strict(xml).unwrap_err();
            assert!(err.contains(why), "{xml}: {err}");
            // The lexer is the same in both modes.
            assert!(scan_osc_text(xml, |_, _, _| {}).is_err(), "{xml}");
        }
    }
}
