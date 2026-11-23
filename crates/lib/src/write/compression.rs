//! Compression algorithm spec for PBF output blobs.

use std::str::FromStr;

/// A zlib compression level, checked to lie in 0-9 when it is built.
///
/// The zlib-rs backend behind flate2 panics on any other level when it
/// builds a compressor, so the range is a property of the type: every
/// `Compression::Zlib` the writer can be handed carries a level the backend
/// accepts, and no write path needs a check of its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ZlibLevel(u32);

impl ZlibLevel {
    /// The highest zlib level.
    pub const MAX: u32 = 9;
    /// Level 6, the default; matches osmium's (`Z_DEFAULT_COMPRESSION`).
    pub const DEFAULT: Self = Self(6);
    /// Level 1, the fastest level that compresses.
    pub const FAST: Self = Self(1);

    /// A zlib level, refused outside 0-9.
    pub fn new(level: u32) -> Result<Self, CompressionError> {
        if level > Self::MAX {
            return Err(CompressionError(format!(
                "zlib level must be 0-{}, got {level}",
                Self::MAX
            )));
        }
        Ok(Self(level))
    }

    /// The level as an integer.
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// A zstd compression level, checked against the linked zstd library's own
/// supported range (`ZSTD_minCLevel()` to `ZSTD_maxCLevel()`) when it is
/// built. Zero selects the backend default; negative levels favour speed.
///
/// The bounds are the library's, not a narrower pbfhogg policy: zstd clamps
/// any level outside them, so a refusal here replaces a silent clamp.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ZstdLevel(i32);

impl ZstdLevel {
    /// Level 3, zstd's own default and the bare `zstd` spec's level.
    pub const DEFAULT: Self = Self(3);

    /// A zstd level, refused outside the linked library's supported range.
    pub fn new(level: i32) -> Result<Self, CompressionError> {
        let (min, max) = Self::range();
        if !(min..=max).contains(&level) {
            return Err(CompressionError(format!(
                "zstd level must be {min}..{max}, got {level}"
            )));
        }
        Ok(Self(level))
    }

    /// The inclusive range of levels the linked zstd library supports.
    pub fn range() -> (i32, i32) {
        (
            zstd::zstd_safe::min_c_level(),
            zstd::zstd_safe::max_c_level(),
        )
    }

    /// The level as an integer.
    pub const fn get(self) -> i32 {
        self.0
    }
}

/// Compression algorithm for PBF output blobs.
///
/// The levels are checked types, so a value of this enum always names a
/// level its backend accepts; build one with [`Compression::zlib`],
/// [`Compression::zstd`], the level types' constructors, or by parsing a
/// spec string.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Compression {
    /// No compression (raw bytes).
    None,
    /// Zlib compression at the given level (0-9).
    Zlib(ZlibLevel),
    /// Zstd compression at the given level, default 3.
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
    Zstd(ZstdLevel),
}

impl Compression {
    /// Zlib at `level`, refused outside 0-9.
    pub fn zlib(level: u32) -> Result<Self, CompressionError> {
        ZlibLevel::new(level).map(Self::Zlib)
    }

    /// Zstd at `level`, refused outside the linked zstd library's range.
    pub fn zstd(level: i32) -> Result<Self, CompressionError> {
        ZstdLevel::new(level).map(Self::Zstd)
    }
}

impl Default for Compression {
    fn default() -> Self {
        // The parser's bare "zlib" resolves to this value, and every CLI
        // compression default is spelled "zlib". OSC gzip uses a separate
        // level-1 policy; it does not consume this setting.
        Compression::Zlib(ZlibLevel::DEFAULT)
    }
}

/// A refused compression spec or level.
#[derive(Debug, Clone)]
pub struct CompressionError(String);

impl std::fmt::Display for CompressionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CompressionError {}

impl FromStr for Compression {
    type Err = CompressionError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "none" => Ok(Self::None),
            "zlib" => Ok(Self::default()),
            "zstd" => Ok(Self::Zstd(ZstdLevel::DEFAULT)),
            _ if s.starts_with("zlib:") => {
                let level: u32 = s[5..]
                    .parse()
                    .map_err(|_| CompressionError(format!("invalid zlib level: {s}")))?;
                Self::zlib(level)
            }
            _ if s.starts_with("zstd:") => {
                let level: i32 = s[5..]
                    .parse()
                    .map_err(|_| CompressionError(format!("invalid zstd level: {s}")))?;
                Self::zstd(level)
            }
            _ => Err(CompressionError(format!(
                "unknown compression: {s} (expected none, zlib, zlib:LEVEL, zstd, zstd:LEVEL)"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_are_valid_levels() {
        assert_eq!(ZlibLevel::new(6).expect("level 6"), ZlibLevel::DEFAULT);
        assert_eq!(ZlibLevel::new(1).expect("level 1"), ZlibLevel::FAST);
        assert_eq!(ZstdLevel::new(3).expect("level 3"), ZstdLevel::DEFAULT);
    }

    #[test]
    fn direct_construction_refuses_out_of_range_levels() {
        assert!(Compression::zlib(10).is_err());
        assert!(Compression::zlib(u32::MAX).is_err());
        let (min, max) = ZstdLevel::range();
        assert!(min < -7, "zstd supports levels below -7, got min {min}");
        assert!(Compression::zstd(min).is_ok());
        assert!(Compression::zstd(max).is_ok());
        assert!(Compression::zstd(min - 1).is_err());
        assert!(Compression::zstd(max + 1).is_err());
    }

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
                Compression::zlib(level).expect("valid zlib level"),
            );
        }
        let (min, max) = ZstdLevel::range();
        for level in [min, -8, -7, 0, max] {
            assert_eq!(
                format!("zstd:{level}")
                    .parse::<Compression>()
                    .expect("valid zstd level"),
                Compression::zstd(level).expect("valid zstd level"),
            );
        }
        let below = format!("zstd:{}", min - 1);
        let above = format!("zstd:{}", max + 1);
        for invalid in ["zlib:-1", "zlib:10", below.as_str(), above.as_str()] {
            assert!(invalid.parse::<Compression>().is_err(), "{invalid}");
        }
        assert_eq!(
            "zstd"
                .parse::<Compression>()
                .expect("valid compression spec"),
            Compression::Zstd(ZstdLevel::DEFAULT),
        );
    }
}
