//! Builder for the `OSMHeader` blob that starts every PBF file.

use std::io;

use protohoggr::{encode_bytes_field, encode_int64_field_always, encode_sint64_field_always};

/// Builder for constructing the `OSMHeader` blob that starts every PBF file.
///
/// Use [`new`](Self::new) for a blank header, or [`from_header`](Self::from_header)
/// to copy bbox and replication metadata from an existing [`HeaderBlock`].
///
/// [`HeaderBlock`]: crate::HeaderBlock
///
/// # Examples
///
/// ```rust
/// use pbfhogg::block_builder::HeaderBuilder;
///
/// // Minimal header (tests, quick scripts)
/// let bytes = HeaderBuilder::new().build()?;
///
/// // Sorted PBF with bounding box
/// let bytes = HeaderBuilder::new()
///     .bbox(9.0, 54.0, 13.0, 58.0)
///     .sorted()
///     .build()?;
/// # Ok::<(), pbfhogg::Error>(())
/// ```
pub struct HeaderBuilder<'a> {
    bbox: Option<(f64, f64, f64, f64)>,
    replication_timestamp: Option<i64>,
    replication_sequence_number: Option<i64>,
    replication_base_url: Option<&'a str>,
    source: Option<&'a str>,
    optional_features: Vec<&'a str>,
    historical_information: bool,
    sorted: bool,
    writing_program: &'a str,
}

impl Default for HeaderBuilder<'_> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a> HeaderBuilder<'a> {
    /// Create a blank header builder.
    ///
    /// The writing program defaults to `"pbfhogg"`. Required features
    /// (`OsmSchema-V0.6`, `DenseNodes`) are always included.
    #[must_use]
    pub fn new() -> Self {
        HeaderBuilder {
            bbox: None,
            replication_timestamp: None,
            replication_sequence_number: None,
            replication_base_url: None,
            source: None,
            optional_features: Vec::new(),
            historical_information: false,
            sorted: false,
            writing_program: "pbfhogg",
        }
    }

    /// Create a header builder pre-populated with bbox, `source`, and
    /// replication metadata from an existing [`HeaderBlock`].
    ///
    /// Optional features (including `Sort.Type_then_ID`) are **not** copied -
    /// call [`.sorted()`](Self::sorted) explicitly if the output should declare
    /// sorted order. This is deliberate hygiene: a command that rewrites way
    /// payloads or way-blob headers without maintaining WayMembers-v1 and
    /// SharedNodePins-v1 must not preserve their feature declarations.
    ///
    /// [`HeaderBlock`]: crate::HeaderBlock
    #[must_use]
    pub fn from_header(header: &'a crate::HeaderBlock) -> Self {
        let mut hb = Self::new();
        if let Some(b) = header.bbox() {
            hb.bbox = Some((b.left, b.bottom, b.right, b.top));
        }
        hb.replication_timestamp = header.osmosis_replication_timestamp();
        hb.replication_sequence_number = header.osmosis_replication_sequence_number();
        hb.replication_base_url = header.osmosis_replication_base_url();
        hb.source = header.source();
        hb.historical_information = header.has_historical_information();
        hb
    }

    /// Set the bounding box (left/bottom/right/top in degrees).
    #[must_use]
    pub fn bbox(mut self, left: f64, bottom: f64, right: f64, top: f64) -> Self {
        self.bbox = Some((left, bottom, right, top));
        self
    }

    /// Clear any bounding box carried over from [`from_header`](Self::from_header).
    ///
    /// The built header omits field 1 entirely, so a consumer sees a file
    /// with no declared bbox. Used by `degrade --strip-bbox` to produce an
    /// adversarial input whose file-level extent is absent.
    #[must_use]
    pub fn without_bbox(mut self) -> Self {
        self.bbox = None;
        self
    }

    /// Set the replication timestamp (seconds since UNIX epoch).
    #[must_use]
    pub fn replication_timestamp(mut self, ts: i64) -> Self {
        self.replication_timestamp = Some(ts);
        self
    }

    /// Set the replication sequence number.
    #[must_use]
    pub fn replication_sequence_number(mut self, seq: i64) -> Self {
        self.replication_sequence_number = Some(seq);
        self
    }

    /// Set the `source` string (HeaderBlock field 17).
    #[must_use]
    pub fn source(mut self, source: &'a str) -> Self {
        self.source = Some(source);
        self
    }

    /// Clear any `source` carried over from [`from_header`](Self::from_header).
    #[must_use]
    pub fn without_source(mut self) -> Self {
        self.source = None;
        self
    }

    /// Set the replication base URL.
    #[must_use]
    pub fn replication_base_url(mut self, url: &'a str) -> Self {
        self.replication_base_url = Some(url);
        self
    }

    /// Declare `Sort.Type_then_ID` - elements are sorted by type then by ID.
    #[must_use]
    pub fn sorted(mut self) -> Self {
        self.sorted = true;
        self
    }

    /// Add an arbitrary optional feature string (e.g. `"LocationsOnWays"`).
    ///
    /// For `Sort.Type_then_ID`, prefer the type-safe [`.sorted()`](Self::sorted)
    /// method instead.
    #[must_use]
    pub fn optional_feature(mut self, feature: &'a str) -> Self {
        self.optional_features.push(feature);
        self
    }

    /// Declare that element history metadata (`visible`) may be present.
    ///
    /// This adds required feature `HistoricalInformation` to the header.
    #[must_use]
    pub fn historical(mut self) -> Self {
        self.historical_information = true;
        self
    }

    /// Override the writing program name (default: `"pbfhogg"`).
    #[must_use]
    pub fn writing_program(mut self, program: &'a str) -> Self {
        self.writing_program = program;
        self
    }

    /// Serialize the header into protobuf bytes suitable for
    /// [`PbfWriter::write_header`](crate::writer::PbfWriter::write_header).
    ///
    /// HeaderBlock fields: bbox (submessage, field 1),
    /// required_features (repeated string, field 4),
    /// optional_features (repeated string, field 5),
    /// writingprogram (string, field 16), source (string, field 17),
    /// osmosis_replication_timestamp (int64, field 32),
    /// osmosis_replication_sequence_number (int64, field 33),
    /// osmosis_replication_base_url (string, field 34).
    #[allow(clippy::cast_possible_truncation)]
    pub fn build(self) -> io::Result<Vec<u8>> {
        let mut buf = Vec::new();

        // Field 1: bbox (HeaderBBox submessage, optional)
        // HeaderBBox: left (sint64, field 1), right (sint64, field 2),
        //             top (sint64, field 3), bottom (sint64, field 4).
        if let Some((left, bottom, right, top)) = self.bbox {
            let mut bbox_buf = Vec::new();
            encode_sint64_field_always(&mut bbox_buf, 1, (left * 1e9).round() as i64);
            encode_sint64_field_always(&mut bbox_buf, 2, (right * 1e9).round() as i64);
            encode_sint64_field_always(&mut bbox_buf, 3, (top * 1e9).round() as i64);
            encode_sint64_field_always(&mut bbox_buf, 4, (bottom * 1e9).round() as i64);
            encode_bytes_field(&mut buf, 1, &bbox_buf);
        }

        // Field 4: required_features (repeated string)
        encode_bytes_field(&mut buf, 4, b"OsmSchema-V0.6");
        encode_bytes_field(&mut buf, 4, b"DenseNodes");
        if self.historical_information {
            encode_bytes_field(
                &mut buf,
                4,
                crate::HeaderBlock::HISTORICAL_INFORMATION.as_bytes(),
            );
        }

        // Field 5: optional_features (repeated string)
        if self.sorted {
            encode_bytes_field(
                &mut buf,
                5,
                crate::HeaderBlock::SORT_TYPE_THEN_ID.as_bytes(),
            );
        }
        for feature in &self.optional_features {
            encode_bytes_field(&mut buf, 5, feature.as_bytes());
        }

        // Field 16: writingprogram (string)
        encode_bytes_field(&mut buf, 16, self.writing_program.as_bytes());

        // Field 17: source (string)
        if let Some(source) = self.source {
            encode_bytes_field(&mut buf, 17, source.as_bytes());
        }

        // Fields 32/33: replication timestamp and sequence number (int64).
        // `_always`: the presence decision is the `Option`, not the value.
        // `encode_int64_field` skips zero, which silently dropped replication
        // sequence number 0 - a real sequence number, and the one a fresh
        // replication chain starts at.
        if let Some(ts) = self.replication_timestamp {
            encode_int64_field_always(&mut buf, 32, ts);
        }

        if let Some(seq) = self.replication_sequence_number {
            encode_int64_field_always(&mut buf, 33, seq);
        }

        // Field 34: osmosis_replication_base_url (string)
        if let Some(url) = self.replication_base_url {
            encode_bytes_field(&mut buf, 34, url.as_bytes());
        }

        Ok(buf)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::HeaderBuilder;

    /// Minimal protobuf field walker, written from the wire format rather
    /// than built on this crate's reader.
    ///
    /// A builder test that decodes its own output with our `HeaderBlock`
    /// parser proves only that the two agree. This walks tag/varint/length
    /// bytes directly, so a field that is not on the wire cannot be
    /// hallucinated into existence by a shared bug.
    fn fields(buf: &[u8]) -> Vec<(u32, u32, Vec<u8>)> {
        let mut out = Vec::new();
        let mut i = 0usize;
        while i < buf.len() {
            let (tag, n) = varint(buf, i);
            i += n;
            let field = u32::try_from(tag >> 3).unwrap();
            let wire = u32::try_from(tag & 7).unwrap();
            match wire {
                0 => {
                    let (v, n) = varint(buf, i);
                    i += n;
                    out.push((field, wire, v.to_le_bytes().to_vec()));
                }
                2 => {
                    let (len, n) = varint(buf, i);
                    i += n;
                    let len = usize::try_from(len).unwrap();
                    out.push((field, wire, buf[i..i + len].to_vec()));
                    i += len;
                }
                other => panic!("unexpected wire type {other} for field {field}"),
            }
        }
        out
    }

    fn varint(buf: &[u8], mut i: usize) -> (u64, usize) {
        let start = i;
        let mut value = 0u64;
        let mut shift = 0u32;
        loop {
            let byte = buf[i];
            i += 1;
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return (value, i - start);
            }
            shift += 7;
        }
    }

    fn field_bytes(buf: &[u8], field: u32) -> Option<Vec<u8>> {
        fields(buf)
            .into_iter()
            .find(|(f, _, _)| *f == field)
            .map(|(_, _, v)| v)
    }

    /// `source` must actually be written.
    ///
    /// `build`'s doc listed `source (string, field 17)` among the fields it
    /// writes, and no code path emitted field 17 at all - so every command
    /// that rewrote a header through `HeaderBuilder` dropped it. The durable
    /// doc was false in the direction that loses data.
    #[test]
    fn source_reaches_the_wire_as_field_17() {
        let bytes = HeaderBuilder::new()
            .source("http://www.openstreetmap.org/api")
            .build()
            .unwrap();
        assert_eq!(
            field_bytes(&bytes, 17).as_deref(),
            Some(b"http://www.openstreetmap.org/api".as_slice()),
            "field 17 must carry the source string verbatim"
        );
        // Absent stays absent - the field is optional, not defaulted.
        let bare = HeaderBuilder::new().build().unwrap();
        assert!(
            field_bytes(&bare, 17).is_none(),
            "a header with no source must not emit an empty field 17"
        );
    }

    /// `from_header` must carry `source` across a header rewrite.
    ///
    /// This is the path every command that rebuilds a header takes, and it
    /// copied bbox / replication / historical but not field 17.
    #[test]
    fn from_header_carries_source_across_a_rewrite() {
        let original = HeaderBuilder::new()
            .source("geofabrik.de")
            .replication_sequence_number(0)
            .build()
            .unwrap();
        let parsed = crate::HeaderBlock::parse_from_bytes(&original).unwrap();
        let rebuilt = HeaderBuilder::from_header(&parsed).build().unwrap();
        assert_eq!(
            field_bytes(&rebuilt, 17).as_deref(),
            Some(b"geofabrik.de".as_slice()),
            "a header rewritten through from_header must keep its source"
        );
        assert_eq!(
            field_bytes(&rebuilt, 33).as_deref(),
            Some(0u64.to_le_bytes().as_slice()),
            "and must keep replication sequence number 0"
        );
        // `without_source` is the opt-out, and must actually opt out.
        let stripped = HeaderBuilder::from_header(&parsed)
            .without_source()
            .build()
            .unwrap();
        assert!(field_bytes(&stripped, 17).is_none());
    }

    /// Replication sequence number 0 is a real sequence number.
    ///
    /// Both replication fields went through `encode_int64_field`, which skips
    /// zero. The `Option` is the presence decision; the value must not
    /// second-guess it.
    #[test]
    fn replication_sequence_number_zero_is_not_dropped() {
        let bytes = HeaderBuilder::new()
            .replication_sequence_number(0)
            .replication_timestamp(0)
            .build()
            .unwrap();
        assert_eq!(
            field_bytes(&bytes, 33).as_deref(),
            Some(0u64.to_le_bytes().as_slice()),
            "sequence number 0 must be on the wire"
        );
        assert_eq!(
            field_bytes(&bytes, 32).as_deref(),
            Some(0u64.to_le_bytes().as_slice()),
            "replication timestamp 0 (the epoch) must be on the wire too"
        );
        let bare = HeaderBuilder::new().build().unwrap();
        assert!(
            field_bytes(&bare, 33).is_none() && field_bytes(&bare, 32).is_none(),
            "an unset Option must still emit nothing"
        );
    }
}
