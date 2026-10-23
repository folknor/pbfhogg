//! Compression algorithm spec for PBF output blobs.

use std::str::FromStr;

/// Compression algorithm for PBF output blobs.
///
/// Parsing validates the application's level ranges. Direct enum construction
/// retains the public integer payloads for compatibility and delegates to the
/// backend: flate2/zlib-rs accepts only 0-9 and panics on any other level when
/// it builds the compressor for the first zlib blob (on the framing worker for
/// pipelined writers); zstd clamps compression levels to its own supported
/// range (which extends below the parser's -7 floor). Replacing
/// these payloads with checked newtypes would break callers constructing or
/// matching variants even though this enum is non-exhaustive. Such a migration
/// must update the writer backends and callers together, rather than silently
/// narrowing the existing direct-construction API.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Compression {
    /// No compression (raw bytes).
    None,
    /// Zlib compression at the given level (0-9).
    /// Level 6 matches osmium's default (`Z_DEFAULT_COMPRESSION`).
    Zlib(u32),
    /// Zstd compression; parsed levels are -7 through 22, default 3.
    /// Zero selects the backend default; negative levels favor speed.
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
        // The parser's bare "zlib" resolves to this value, and every CLI
        // compression default (bench-write included) is spelled "zlib", so
        // the write benchmark measures the level commands write. OSC gzip
        // uses a separate level-1 policy; it does not consume this setting.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parsed_defaults_and_level_boundaries() {
        assert_eq!(
            "zlib"
                .parse::<Compression>()
                .expect("valid compression spec"),
            Compression::default(),
        );
        assert_eq!(
            "zlib:6"
                .parse::<Compression>()
                .expect("valid compression spec"),
            Compression::default(),
        );
        for level in [0, 9] {
            assert_eq!(
                format!("zlib:{level}")
                    .parse::<Compression>()
                    .expect("valid zlib level"),
                Compression::Zlib(level),
            );
        }
        for level in [-7, 0, 22] {
            assert_eq!(
                format!("zstd:{level}")
                    .parse::<Compression>()
                    .expect("valid zstd level"),
                Compression::Zstd(level),
            );
        }
        for invalid in ["zlib:-1", "zlib:10", "zstd:-8", "zstd:23"] {
            assert!(invalid.parse::<Compression>().is_err(), "{invalid}");
        }
        assert_eq!(
            "zstd"
                .parse::<Compression>()
                .expect("valid compression spec"),
            Compression::Zstd(3),
        );
    }
}
