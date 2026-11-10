use std::io::{self, Write};

use super::ExportFormat;

pub(super) struct FeatureWriter<W> {
    out: W,
    format: ExportFormat,
    wrote_any: bool,
    buf: String,
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
            buf: String::new(),
        })
    }

    pub(super) fn write_feature_geometry_props(
        &mut self,
        geometry: &str,
        properties: &str,
    ) -> io::Result<()> {
        self.buf.clear();
        self.buf.push_str("{\"type\":\"Feature\",\"geometry\":");
        self.buf.push_str(geometry);
        self.buf.push_str(",\"properties\":");
        self.buf.push_str(properties);
        self.buf.push('}');

        match self.format {
            ExportFormat::GeoJsonSeq => {
                self.out.write_all(self.buf.as_bytes())?;
                self.out.write_all(b"\n")?;
            }
            ExportFormat::GeoJson => {
                if self.wrote_any {
                    self.out.write_all(b",")?;
                }
                self.out.write_all(self.buf.as_bytes())?;
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

    #[test]
    fn frames_sequence() {
        let mut out = Vec::new();
        let mut writer = FeatureWriter::new(&mut out, ExportFormat::GeoJsonSeq).expect("writer");
        writer
            .write_feature_geometry_props("null", "{}")
            .expect("feature");
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
        writer
            .write_feature_geometry_props("null", "{}")
            .expect("first");
        writer
            .write_feature_geometry_props("null", "{}")
            .expect("second");
        writer.finish().expect("finish");
        assert_eq!(
            out,
            b"{\"type\":\"FeatureCollection\",\"features\":[{\"type\":\"Feature\",\"geometry\":null,\"properties\":{}},{\"type\":\"Feature\",\"geometry\":null,\"properties\":{}}]}\n"
        );
    }
}
