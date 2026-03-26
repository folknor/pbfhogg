# nidhogg::pipeline

The processing pipeline: reading, transforming, and writing tiles.

## Traits

### `TileProcessor`

The core trait for implementing custom tile processing stages. Each stage receives tiles from the previous stage and produces tiles for the next.

```rust
pub trait TileProcessor: Send + Sync {
    /// Process a single tile, returning zero or more output tiles.
    ///
    /// Returning an empty vec drops the tile from the pipeline.
    /// Returning multiple tiles is useful for splitting operations.
    fn process(&self, tile: Tile) -> Result<Vec<Tile>, PipelineError>;

    /// Human-readable name for logging and progress reporting.
    fn name(&self) -> &str;

    /// Called once before processing begins. Use for initialization
    /// that depends on the full config (e.g., loading shapefiles).
    fn init(&mut self, _config: &Config) -> Result<(), PipelineError> {
        Ok(())
    }

    /// Called once after all tiles have been processed.
    fn finish(&mut self) -> Result<(), PipelineError> {
        Ok(())
    }
}
```

#### Example

```rust
use nidhogg::pipeline::{TileProcessor, PipelineError};
use nidhogg::tile::Tile;

struct DropEmptyLayers;

impl TileProcessor for DropEmptyLayers {
    fn process(&self, mut tile: Tile) -> Result<Vec<Tile>, PipelineError> {
        tile.layers.retain(|layer| !layer.is_empty());
        if tile.layers.is_empty() {
            Ok(vec![])  // drop tile entirely
        } else {
            Ok(vec![tile])
        }
    }

    fn name(&self) -> &str {
        "drop-empty-layers"
    }
}
```

### `TileSource`

Produces tiles for the pipeline. Implemented by PBF readers and other input formats.

```rust
pub trait TileSource: Send {
    /// Iterate over all tiles this source can produce.
    fn tiles(&self) -> Box<dyn Iterator<Item = Result<Tile, PipelineError>> + Send>;

    /// Total number of tiles, if known in advance.
    /// Used for progress reporting.
    fn size_hint(&self) -> Option<u64>;
}
```

### `TileSink`

Consumes tiles at the end of the pipeline. Implemented by PMTiles/MBTiles writers.

```rust
pub trait TileSink: Send {
    /// Write a single tile to the output.
    fn write_tile(&mut self, tile: &Tile) -> Result<(), PipelineError>;

    /// Finalize the output (write headers, flush buffers, etc.).
    fn finalize(&mut self) -> Result<TileStats, PipelineError>;
}
```

## Structs

### `Pipeline`

Orchestrates the full processing flow: source -> processors -> sink.

```rust
pub struct Pipeline {
    source: Box<dyn TileSource>,
    processors: Vec<Box<dyn TileProcessor>>,
    sink: Box<dyn TileSink>,
    config: Config,
}
```

#### Methods

```rust
impl Pipeline {
    /// Create a new pipeline with the given configuration.
    pub fn new(config: Config) -> Result<Self, PipelineError>

    /// Create a pipeline from standard components based on config.
    ///
    /// This is the main entry point for typical usage. It sets up
    /// the PBF reader, Shortbread processor chain, and output writer
    /// based on the configuration.
    pub fn from_config(config: Config) -> Result<Self, PipelineError>

    /// Add a custom processing stage to the pipeline.
    pub fn add_processor(&mut self, processor: impl TileProcessor + 'static)

    /// Insert a processing stage at a specific position.
    pub fn insert_processor(&mut self, index: usize, processor: impl TileProcessor + 'static)

    /// Run the pipeline to completion.
    ///
    /// Returns statistics about the run (tiles processed, bytes written, etc.).
    /// Progress is reported via the `tracing` crate.
    pub fn run(self) -> Result<TileStats, PipelineError>

    /// Run the pipeline with a progress callback.
    pub fn run_with_progress<F>(self, callback: F) -> Result<TileStats, PipelineError>
    where
        F: Fn(ProgressEvent) + Send + 'static
}
```

#### Example

```rust
use nidhogg::{Config, Pipeline};

let config = Config::from_file("nidhogg.toml")?;
let mut pipeline = Pipeline::from_config(config)?;

// Optionally add custom stages
pipeline.add_processor(DropEmptyLayers);

let stats = pipeline.run()?;
println!("Processed {} tiles ({} bytes)", stats.tiles, stats.bytes_written);
```

### `TileStats`

Statistics returned after a pipeline run completes.

```rust
pub struct TileStats {
    /// Total tiles processed.
    pub tiles: u64,
    /// Total tiles written (after filtering).
    pub tiles_written: u64,
    /// Total bytes written to output.
    pub bytes_written: u64,
    /// Total features processed across all tiles.
    pub features: u64,
    /// Per-layer feature counts.
    pub layer_counts: HashMap<String, u64>,
    /// Processing wall-clock time.
    pub duration: Duration,
    /// Peak memory usage in bytes.
    pub peak_memory: u64,
}
```

#### Methods

```rust
impl TileStats {
    /// Print a human-readable summary to stderr.
    pub fn print_summary(&self)

    /// Serialize to JSON for machine consumption.
    pub fn to_json(&self) -> serde_json::Value
}
```

### `ProgressEvent`

Reported during pipeline execution for progress tracking.

```rust
pub enum ProgressEvent {
    /// Pipeline stage started.
    StageStarted { name: String },
    /// A batch of tiles was processed.
    TilesProcessed { count: u64, total: Option<u64> },
    /// Pipeline stage completed.
    StageCompleted { name: String, duration: Duration },
    /// Sort pass started/completed.
    SortPass { pass: u32, total: u32 },
}
```

## Enums

### `PipelineError`

```rust
#[derive(Debug, thiserror::Error)]
pub enum PipelineError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("PBF decode error: {0}")]
    PbfDecode(String),

    #[error("invalid geometry at feature {id}: {reason}")]
    InvalidGeometry { id: u64, reason: String },

    #[error("tile encoding failed: {0}")]
    EncodingError(String),

    #[error("configuration error: {0}")]
    Config(#[from] ConfigError),

    #[error("plugin error in {plugin}: {message}")]
    Plugin { plugin: String, message: String },

    #[error("out of memory (limit: {limit} bytes, needed: {needed} bytes)")]
    OutOfMemory { limit: u64, needed: u64 },
}
```

## Free Functions

### `shortbread_processors`

Returns the default Shortbread processing chain as a vec of processors.

```rust
/// Create the standard Shortbread tile processing stages.
///
/// The returned processors handle:
/// 1. OSM tag matching against Shortbread layer rules
/// 2. Geometry simplification per zoom level
/// 3. Feature deduplication
/// 4. Hilbert curve sorting
///
/// Use this to build a custom pipeline that includes the standard
/// stages plus your own additions.
pub fn shortbread_processors(config: &Config) -> Vec<Box<dyn TileProcessor>>
```

### `read_pbf`

```rust
/// Open an OSM PBF file and return a TileSource.
///
/// The source reads the PBF in parallel using rayon, decoding
/// blocks on worker threads. Features are extracted and assigned
/// to tiles based on their geometry.
pub fn read_pbf(path: impl AsRef<Path>, config: &Config) -> Result<Box<dyn TileSource>, PipelineError>
```

### `write_pmtiles` / `write_mbtiles`

```rust
/// Create a PMTiles TileSink.
pub fn write_pmtiles(path: impl AsRef<Path>, config: &Config) -> Result<Box<dyn TileSink>, PipelineError>

/// Create an MBTiles TileSink.
pub fn write_mbtiles(path: impl AsRef<Path>, config: &Config) -> Result<Box<dyn TileSink>, PipelineError>
```
