use std::io::{self, Write};

use super::ExportFormat;

/// Frames formatted feature bodies into the output. Every feature is
/// offered to `out` as it always was: the body then its newline for a
/// sequence, the separating comma then the body for a collection, so a
/// caller's writer sees the same calls whichever thread formatted them.
pub(super) struct FeatureWriter<W> {
    out: W,
    format: ExportFormat,
    wrote_any: bool,
}

impl<W: Write> FeatureWriter<W> {
    pub(super) fn new(mut out: W, format: ExportFormat) -> io::Result<Self> {
        if matches!(format, ExportFormat::GeoJson) {
            out.write_all(b"{\"type\":\"FeatureCollection\",\"features\":[")?;
        }
        Ok(Self {
            out,
            format,
            wrote_any: false,
        })
    }

    /// Writes one complete feature body.
    pub(super) fn write_feature(&mut self, body: &[u8]) -> io::Result<()> {
        match self.format {
            ExportFormat::GeoJsonSeq => {
                self.out.write_all(body)?;
                self.out.write_all(b"\n")?;
            }
            ExportFormat::GeoJson => {
                if self.wrote_any {
                    self.out.write_all(b",")?;
                }
                self.out.write_all(body)?;
            }
        }
        self.wrote_any = true;
        Ok(())
    }

    pub(super) fn finish(mut self) -> io::Result<()> {
        if matches!(self.format, ExportFormat::GeoJson) {
            self.out.write_all(b"]}\n")?;
        }
        self.out.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FEATURE: &[u8] = b"{\"type\":\"Feature\",\"geometry\":null,\"properties\":{}}";

    #[test]
    fn frames_sequence() {
        let mut out = Vec::new();
        let mut writer = FeatureWriter::new(&mut out, ExportFormat::GeoJsonSeq).expect("writer");
        writer.write_feature(FEATURE).expect("feature");
        writer.finish().expect("finish");
        assert_eq!(
            out,
            b"{\"type\":\"Feature\",\"geometry\":null,\"properties\":{}}\n"
        );
    }

    #[test]
    fn frames_collection_and_empty_collection() {
        let mut out = Vec::new();
        FeatureWriter::new(&mut out, ExportFormat::GeoJson)
            .expect("writer")
            .finish()
            .expect("finish");
        assert_eq!(out, b"{\"type\":\"FeatureCollection\",\"features\":[]}\n");

        out.clear();
        let mut writer = FeatureWriter::new(&mut out, ExportFormat::GeoJson).expect("writer");
        writer.write_feature(FEATURE).expect("first");
        writer.write_feature(FEATURE).expect("second");
        writer.finish().expect("finish");
        assert_eq!(
            out,
            b"{\"type\":\"FeatureCollection\",\"features\":[{\"type\":\"Feature\",\"geometry\":null,\"properties\":{}},{\"type\":\"Feature\",\"geometry\":null,\"properties\":{}}]}\n"
        );
    }
}
