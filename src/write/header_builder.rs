//! Builder for the `OSMHeader` blob that starts every PBF file.

use crate::read::block::Feature;
use crate::read::wire_schema::{header_bbox, header_block};

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
    // True when `bbox` was copied from input headers rather than set by
    // the caller. Input boxes are re-serialized as read (an out-of-range
    // or inverted box in an input file round-trips, as it did before
    // validation existed); only caller-set boxes get the range and order
    // checks in `build`.
    bbox_from_input: bool,
    replication_timestamp: Option<i64>,
    replication_sequence_number: Option<i64>,
    replication_base_url: Option<&'a str>,
    source: Option<&'a str>,
    optional_features: Vec<Feature<'a>>,
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
            bbox_from_input: false,
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
            hb = hb.input_bbox(b.left, b.bottom, b.right, b.top);
        }
        hb.replication_timestamp = header.osmosis_replication_timestamp();
        hb.replication_sequence_number = header.osmosis_replication_sequence_number();
        hb.replication_base_url = header.osmosis_replication_base_url();
        hb.source = header.source();
        hb.historical_information = header.has_historical_information();
        hb
    }

    /// Set the bounding box (left/bottom/right/top in degrees).
    ///
    /// [`build`](Self::build) returns `InvalidInput` if a coordinate is
    /// non-finite, outside the usual longitude/latitude bounds, or if either
    /// minimum is greater than its maximum. A box copied by
    /// [`from_header`](Self::from_header) is re-serialized as read instead;
    /// calling this replaces it and subjects the new box to the checks.
    #[must_use]
    pub fn bbox(mut self, left: f64, bottom: f64, right: f64, top: f64) -> Self {
        self.bbox = Some((left, bottom, right, top));
        self.bbox_from_input = false;
        self
    }

    /// Set a bounding box derived from input headers (a copy or a union of
    /// them). [`build`](Self::build) still refuses a non-finite coordinate,
    /// but serializes an out-of-range or inverted box as given: refusing an
    /// input file for its header extent would turn a passthrough into a
    /// failure on data the command did not author.
    #[must_use]
    pub(crate) fn input_bbox(mut self, left: f64, bottom: f64, right: f64, top: f64) -> Self {
        self.bbox = Some((left, bottom, right, top));
        self.bbox_from_input = true;
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

    /// Add a known feature with its [`Feature`] constant, or an arbitrary
    /// extension feature string.
    ///
    /// For `Sort.Type_then_ID`, prefer the type-safe [`.sorted()`](Self::sorted)
    /// method instead.
    #[must_use]
    pub fn optional_feature(mut self, feature: impl Into<Feature<'a>>) -> Self {
        self.optional_features.push(feature.into());
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
    ///
    /// Returns `InvalidInput` when the bounding box has a non-finite
    /// coordinate, or when a box set through [`bbox`](Self::bbox) is out of
    /// range or inverted. A box copied from an input header is not range-
    /// or order-checked; see [`bbox`](Self::bbox).
    #[allow(clippy::cast_possible_truncation)]
    pub fn build(self) -> io::Result<Vec<u8>> {
        // Validate and quantize before writing any bytes. A non-finite
        // value would silently quantize to 0 or saturate, so it is refused
        // whatever its origin (an input header decodes from sint64 and is
        // always finite). Range and order are checked only for caller-set
        // boxes: input boxes round-trip as read.
        let bbox = if let Some((left, bottom, right, top)) = self.bbox {
            if !left.is_finite() || !bottom.is_finite() || !right.is_finite() || !top.is_finite() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "bounding box coordinates must be finite",
                ));
            }
            if !self.bbox_from_input {
                if !(-180.0..=180.0).contains(&left) || !(-180.0..=180.0).contains(&right) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "bounding box longitude must be within [-180, 180] degrees",
                    ));
                }
                if !(-90.0..=90.0).contains(&bottom) || !(-90.0..=90.0).contains(&top) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "bounding box latitude must be within [-90, 90] degrees",
                    ));
                }
                if left > right || bottom > top {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "bounding box minima must not exceed maxima",
                    ));
                }
            }
            Some((
                (left * 1e9).round() as i64,
                (bottom * 1e9).round() as i64,
                (right * 1e9).round() as i64,
                (top * 1e9).round() as i64,
            ))
        } else {
            None
        };

        let mut buf = Vec::new();

        // Field 1: bbox (HeaderBBox submessage, optional)
        // HeaderBBox: left (sint64, field 1), right (sint64, field 2),
        //             top (sint64, field 3), bottom (sint64, field 4).
        if let Some((left, bottom, right, top)) = bbox {
            let mut bbox_buf = Vec::new();
            encode_sint64_field_always(&mut bbox_buf, header_bbox::LEFT, left);
            encode_sint64_field_always(&mut bbox_buf, header_bbox::RIGHT, right);
            encode_sint64_field_always(&mut bbox_buf, header_bbox::TOP, top);
            encode_sint64_field_always(&mut bbox_buf, header_bbox::BOTTOM, bottom);
            encode_bytes_field(&mut buf, header_block::BBOX, &bbox_buf);
        }

        // Field 4: required_features (repeated string)
        encode_bytes_field(
            &mut buf,
            header_block::REQUIRED_FEATURES,
            crate::HeaderBlock::OSM_SCHEMA_V06.as_bytes(),
        );
        encode_bytes_field(
            &mut buf,
            header_block::REQUIRED_FEATURES,
            crate::HeaderBlock::DENSE_NODES.as_bytes(),
        );
        if self.historical_information {
            encode_bytes_field(
                &mut buf,
                header_block::REQUIRED_FEATURES,
                crate::HeaderBlock::HISTORICAL_INFORMATION.as_bytes(),
            );
        }

        // Field 5: optional_features (repeated string)
        if self.sorted {
            encode_bytes_field(
                &mut buf,
                header_block::OPTIONAL_FEATURES,
                crate::HeaderBlock::SORT_TYPE_THEN_ID.as_bytes(),
            );
        }
        for feature in &self.optional_features {
            encode_bytes_field(
                &mut buf,
                header_block::OPTIONAL_FEATURES,
                feature.as_str().as_bytes(),
            );
        }

        // Field 16: writingprogram (string)
        encode_bytes_field(
            &mut buf,
            header_block::WRITING_PROGRAM,
            self.writing_program.as_bytes(),
        );

        // Field 17: source (string)
        if let Some(source) = self.source {
            encode_bytes_field(&mut buf, header_block::SOURCE, source.as_bytes());
        }

        // Fields 32/33: replication timestamp and sequence number (int64).
        // `_always`: the presence decision is the `Option`, not the value.
        // `encode_int64_field` skips zero, which silently dropped replication
        // sequence number 0 - a real sequence number, and the one a fresh
        // replication chain starts at.
        if let Some(ts) = self.replication_timestamp {
            encode_int64_field_always(&mut buf, header_block::REPLICATION_TIMESTAMP, ts);
        }

        if let Some(seq) = self.replication_sequence_number {
            encode_int64_field_always(&mut buf, header_block::REPLICATION_SEQUENCE_NUMBER, seq);
        }

        // Field 34: osmosis_replication_base_url (string)
        if let Some(url) = self.replication_base_url {
            encode_bytes_field(&mut buf, header_block::REPLICATION_BASE_URL, url.as_bytes());
        }

        Ok(buf)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{
        HeaderBuilder, encode_bytes_field, encode_sint64_field_always, header_bbox, header_block,
    };

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

    #[test]
    fn invalid_bounding_boxes_are_rejected() {
        let invalid = [
            (f64::NAN, 0.0, 1.0, 1.0),
            (0.0, 0.0, f64::INFINITY, 1.0),
            (f64::NEG_INFINITY, 0.0, 1.0, 1.0),
            (1.0, 0.0, 0.0, 1.0),
            (0.0, 1.0, 1.0, 0.0),
            (-181.0, 0.0, 1.0, 1.0),
            (0.0, 0.0, 181.0, 1.0),
            (0.0, -91.0, 1.0, 0.0),
            (0.0, 0.0, 1.0, 91.0),
        ];

        for (left, bottom, right, top) in invalid {
            let err = HeaderBuilder::new()
                .bbox(left, bottom, right, top)
                .build()
                .expect_err("invalid bounds must not be serialized");
            assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        }
    }

    #[test]
    fn geographic_bbox_edges_are_accepted() {
        let bytes = HeaderBuilder::new()
            .bbox(-180.0, -90.0, 180.0, 90.0)
            .build()
            .unwrap();
        let parsed = crate::HeaderBlock::parse_from_bytes(&bytes).unwrap();
        let bbox = parsed.bbox().unwrap();
        assert_eq!(
            (bbox.left, bbox.bottom, bbox.right, bbox.top),
            (-180.0, -90.0, 180.0, 90.0)
        );
    }

    /// An input header's box is the input's claim, not ours: an
    /// out-of-range or inverted one must round-trip rather than fail every
    /// command that copies the header.
    #[test]
    fn rebuilding_preserves_an_out_of_range_bbox_from_an_input_header() {
        let mut bbox = Vec::new();
        encode_sint64_field_always(&mut bbox, header_bbox::LEFT, 2_000_000_000);
        encode_sint64_field_always(&mut bbox, header_bbox::RIGHT, 1_000_000_000);
        encode_sint64_field_always(&mut bbox, header_bbox::TOP, 91_000_000_000);
        encode_sint64_field_always(&mut bbox, header_bbox::BOTTOM, 0);

        let mut header_bytes = Vec::new();
        encode_bytes_field(&mut header_bytes, header_block::BBOX, &bbox);
        let parsed = crate::HeaderBlock::parse_from_bytes(&header_bytes).unwrap();
        let rebuilt = HeaderBuilder::from_header(&parsed)
            .build()
            .expect("an input bbox is re-serialized as read");
        let reparsed = crate::HeaderBlock::parse_from_bytes(&rebuilt).unwrap();
        let b = reparsed.bbox().unwrap();
        assert_eq!((b.left, b.bottom, b.right, b.top), (2.0, 0.0, 1.0, 91.0));

        // Replacing it through `bbox` subjects the new box to the checks.
        let err = HeaderBuilder::from_header(&parsed)
            .bbox(0.0, 0.0, 1.0, 91.0)
            .build()
            .expect_err("a caller-set out-of-range bbox must be refused");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }
}
