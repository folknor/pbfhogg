# nidhogg::config

Configuration types and builder API.

## Structs

### `Config`

Top-level configuration for a tile generation run. Created from a TOML file, environment variables, or the builder API.

```rust
pub struct Config {
    pub input: InputConfig,
    pub output: OutputConfig,
    pub processing: ProcessingConfig,
    pub layers: LayerConfig,
    pub simplification: SimplificationConfig,
}
```

#### Methods

```rust
impl Config {
    /// Load configuration from a TOML file.
    ///
    /// Falls back to defaults for any missing fields.
    /// Returns an error if the file exists but cannot be parsed.
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, ConfigError>

    /// Load configuration from environment variables.
    ///
    /// Variables are prefixed with `NIDHOGG_` and nested keys
    /// use double underscores (e.g., `NIDHOGG_OUTPUT__FORMAT`).
    pub fn from_env() -> Result<Self, ConfigError>

    /// Merge this configuration with another, preferring values
    /// from `other` when both are present.
    pub fn merge(self, other: Config) -> Config

    /// Validate the configuration and return a list of warnings.
    ///
    /// Returns `Err` if the configuration is invalid (e.g., min_zoom > max_zoom).
    /// Returns `Ok(warnings)` otherwise, where warnings are non-fatal issues
    /// like unreachable layers at the configured zoom range.
    pub fn validate(&self) -> Result<Vec<ConfigWarning>, ConfigError>
}
```

#### Example

```rust
use nidhogg::config::Config;

let config = Config::builder()
    .output_format(OutputFormat::Pmtiles)
    .max_zoom(14)
    .compression(Compression::Zstd)
    .threads(8)
    .build()?;
```

### `ConfigBuilder`

Builder for constructing `Config` values programmatically.

```rust
pub struct ConfigBuilder { /* fields omitted */ }
```

#### Methods

```rust
impl ConfigBuilder {
    pub fn new() -> Self
    pub fn input_path(self, path: impl Into<PathBuf>) -> Self
    pub fn output_path(self, path: impl Into<PathBuf>) -> Self
    pub fn output_format(self, format: OutputFormat) -> Self
    pub fn compression(self, compression: Compression) -> Self
    pub fn min_zoom(self, zoom: u8) -> Self
    pub fn max_zoom(self, zoom: u8) -> Self
    pub fn threads(self, n: usize) -> Self
    pub fn memory_limit(self, bytes: u64) -> Self
    pub fn temp_dir(self, path: impl Into<PathBuf>) -> Self
    pub fn ocean_shapefile(self, path: impl Into<PathBuf>) -> Self
    pub fn include_layers(self, layers: &[&str]) -> Self
    pub fn exclude_layers(self, layers: &[&str]) -> Self
    pub fn build(self) -> Result<Config, ConfigError>
}
```

### `OutputConfig`

```rust
pub struct OutputConfig {
    pub format: OutputFormat,
    pub min_zoom: u8,
    pub max_zoom: u8,
    pub compression: Compression,
    pub tile_size: u32,
}
```

### `ProcessingConfig`

```rust
pub struct ProcessingConfig {
    pub threads: usize,
    pub memory_limit: u64,
    pub temp_dir: Option<PathBuf>,
    pub batch_size: usize,
}
```

## Enums

### `OutputFormat`

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    /// Single-file archive, servable from cloud storage.
    Pmtiles,
    /// SQLite-based format for traditional tile servers.
    Mbtiles,
    /// Individual .mvt files in a z/x/y directory tree.
    Directory,
}
```

### `Compression`

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    None,
    Gzip,
    Zstd,
    Brotli,
}
```

### `ConfigError`

```rust
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("invalid zoom range: min_zoom ({min}) > max_zoom ({max})")]
    InvalidZoomRange { min: u8, max: u8 },

    #[error("unknown output format: {0}")]
    UnknownFormat(String),

    #[error("failed to parse config file: {0}")]
    ParseError(#[from] toml::de::Error),

    #[error("I/O error reading config: {0}")]
    Io(#[from] std::io::Error),
}
```
