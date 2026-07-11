//! Builder for the `OSMHeader` blob that starts every PBF file.

use std::io;

use protohoggr::{encode_bytes_field, encode_int64_field, encode_sint64_field_always};

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
/// # Ok::<(), std::io::Error>(())
/// ```
pub struct HeaderBuilder<'a> {
    bbox: Option<(f64, f64, f64, f64)>,
    replication_timestamp: Option<i64>,
    replication_sequence_number: Option<i64>,
    replication_base_url: Option<&'a str>,
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
            optional_features: Vec::new(),
            historical_information: false,
            sorted: false,
            writing_program: "pbfhogg",
        }
    }

    /// Create a header builder pre-populated with bbox and replication metadata
    /// from an existing [`HeaderBlock`].
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
        hb.historical_information = header.has_historical_information();
        hb
    }

    /// Set the bounding box (left/bottom/right/top in degrees).
    #[must_use]
    pub fn bbox(mut self, left: f64, bottom: f64, right: f64, top: f64) -> Self {
        self.bbox = Some((left, bottom, right, top));
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

        // Field 32: osmosis_replication_timestamp (int64)
        if let Some(ts) = self.replication_timestamp {
            encode_int64_field(&mut buf, 32, ts);
        }

        // Field 33: osmosis_replication_sequence_number (int64)
        if let Some(seq) = self.replication_sequence_number {
            encode_int64_field(&mut buf, 33, seq);
        }

        // Field 34: osmosis_replication_base_url (string)
        if let Some(url) = self.replication_base_url {
            encode_bytes_field(&mut buf, 34, url.as_bytes());
        }

        Ok(buf)
    }
}
