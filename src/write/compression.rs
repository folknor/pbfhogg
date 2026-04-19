//! Compression algorithm spec for PBF output blobs.

use std::str::FromStr;

/// Compression algorithm for PBF output blobs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Compression {
    /// No compression (raw bytes).
    None,
    /// Zlib compression at the given level (0-9).
    /// Level 6 matches osmium's default (`Z_DEFAULT_COMPRESSION`).
    Zlib(u32),
    /// Zstd compression at the given level (1-22, default 3).
    ///
    /// Zstd decompresses 3-5x faster than zlib at equivalent compression ratios,
    /// making it ideal for read-heavy workflows (planet imports, tile generation).
    /// Level 3 (zstd's default) provides a good balance of compression ratio and
    /// speed. Higher levels (e.g. 19) compress ~10-15% better but are much slower
    /// to write - use for archival PBFs that will be read many times.
    ///
    /// **Compatibility warning:** Not all PBF consumers support zstd yet. As of
    /// 2025, osmium, osm2pgsql, and most tools only read zlib-compressed PBFs.
    /// Use zstd for internal pipelines where you control both writer and reader.
    Zstd(i32),
}

impl Default for Compression {
    fn default() -> Self {
        Compression::Zlib(6)
    }
}

/// Parse error for [`Compression`] string specs.
#[derive(Debug, Clone)]
pub struct ParseCompressionError(String);

impl std::fmt::Display for ParseCompressionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ParseCompressionError {}

impl FromStr for Compression {
    type Err = ParseCompressionError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "none" => Ok(Self::None),
            "zlib" => Ok(Self::default()),
            "zstd" => Ok(Self::Zstd(3)),
            _ if s.starts_with("zlib:") => {
                let level: u32 = s[5..]
                    .parse()
                    .map_err(|_| ParseCompressionError(format!("invalid zlib level: {s}")))?;
                if level > 9 {
                    return Err(ParseCompressionError(format!(
                        "zlib level must be 0-9, got {level}"
                    )));
                }
                Ok(Self::Zlib(level))
            }
            _ if s.starts_with("zstd:") => {
                let level: i32 = s[5..]
                    .parse()
                    .map_err(|_| ParseCompressionError(format!("invalid zstd level: {s}")))?;
                if !(-7..=22).contains(&level) {
                    return Err(ParseCompressionError(format!(
                        "zstd level must be -7..22, got {level}"
                    )));
                }
                Ok(Self::Zstd(level))
            }
            _ => Err(ParseCompressionError(format!(
                "unknown compression: {s} (expected none, zlib, zlib:LEVEL, zstd, zstd:LEVEL)"
            ))),
        }
    }
}
