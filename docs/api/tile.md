# nidhogg::tile

Core tile data types: coordinates, features, layers, and encoded tiles.

## Structs

### `TileCoord`

Identifies a single tile by zoom level and x/y grid position.

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TileCoord {
    pub z: u8,
    pub x: u32,
    pub y: u32,
}
```

#### Methods

```rust
impl TileCoord {
    /// Create a new tile coordinate.
    pub fn new(z: u8, x: u32, y: u32) -> Self

    /// Parse from "z/x/y" string notation.
    pub fn parse(s: &str) -> Result<Self, TileCoordError>

    /// Return the parent tile at zoom - 1.
    /// Returns `None` for zoom level 0.
    pub fn parent(&self) -> Option<TileCoord>

    /// Return the four children at zoom + 1.
    pub fn children(&self) -> [TileCoord; 4]

    /// Return the bounding box in EPSG:4326 coordinates.
    pub fn bounds(&self) -> BBox

    /// Return the total number of tiles at this zoom level.
    pub fn tiles_at_zoom(z: u8) -> u64
}
```

#### Example

```rust
use nidhogg::tile::TileCoord;

let tile = TileCoord::new(14, 8691, 5677);
let bbox = tile.bounds();
println!("Tile covers: {:.4}N, {:.4}E to {:.4}N, {:.4}E",
    bbox.south, bbox.west, bbox.north, bbox.east);

// Navigate the tile tree
let parent = tile.parent().unwrap();
assert_eq!(parent, TileCoord::new(13, 4345, 2838));
```

### `Feature`

A single geographic feature with geometry, properties, and a layer assignment.

```rust
pub struct Feature {
    pub id: u64,
    pub geometry: Geometry,
    pub properties: Properties,
    pub layer: LayerId,
}
```

#### Methods

```rust
impl Feature {
    /// Simplify the feature geometry using Douglas-Peucker.
    pub fn simplify(&mut self, tolerance: f64)

    /// Clip the feature to a bounding box. Returns `None` if
    /// the feature is entirely outside the box.
    pub fn clip(&self, bbox: &BBox) -> Option<Feature>

    /// Encode this feature as MVT protocol buffer bytes.
    pub fn encode_mvt(&self, extent: u32) -> Vec<u8>

    /// Estimated size in bytes when encoded.
    pub fn encoded_size(&self) -> usize
}
```

### `Layer`

A named collection of features within a tile.

```rust
pub struct Layer {
    pub name: String,
    pub features: Vec<Feature>,
    pub extent: u32,
}
```

#### Methods

```rust
impl Layer {
    pub fn new(name: impl Into<String>) -> Self

    /// Add a feature to this layer.
    pub fn push(&mut self, feature: Feature)

    /// Number of features in this layer.
    pub fn len(&self) -> usize

    pub fn is_empty(&self) -> bool

    /// Sort features by Hilbert curve index for improved
    /// compression and spatial locality.
    pub fn sort_by_hilbert(&mut self)

    /// Encode this layer as MVT protocol buffer bytes.
    pub fn encode_mvt(&self) -> Vec<u8>
}
```

### `Tile`

A complete vector tile containing one or more layers.

```rust
pub struct Tile {
    pub coord: TileCoord,
    pub layers: Vec<Layer>,
}
```

#### Methods

```rust
impl Tile {
    pub fn new(coord: TileCoord) -> Self

    /// Add a layer to this tile.
    pub fn add_layer(&mut self, layer: Layer)

    /// Get a layer by name.
    pub fn layer(&self, name: &str) -> Option<&Layer>

    /// Total number of features across all layers.
    pub fn feature_count(&self) -> usize

    /// Encode the full tile as an MVT protobuf.
    pub fn encode(&self) -> Vec<u8>

    /// Encode and compress using the specified algorithm.
    pub fn encode_compressed(&self, compression: Compression) -> Result<Vec<u8>, io::Error>

    /// Decode a tile from MVT protobuf bytes.
    pub fn decode(coord: TileCoord, data: &[u8]) -> Result<Self, DecodeError>
}
```

#### Example

```rust
use nidhogg::tile::{Tile, TileCoord, Layer, Feature};

let mut tile = Tile::new(TileCoord::new(14, 8691, 5677));

let mut roads = Layer::new("transportation");
roads.push(feature);
roads.sort_by_hilbert();

tile.add_layer(roads);

let bytes = tile.encode_compressed(Compression::Zstd)?;
println!("Tile size: {} bytes", bytes.len());
```

## Enums

### `Geometry`

```rust
pub enum Geometry {
    Point(Coord),
    MultiPoint(Vec<Coord>),
    LineString(Vec<Coord>),
    MultiLineString(Vec<Vec<Coord>>),
    Polygon(Vec<Vec<Coord>>),
    MultiPolygon(Vec<Vec<Vec<Coord>>>),
}
```

### `LayerId`

Identifies a Shortbread layer by enum variant rather than string comparison.

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LayerId {
    Ocean,
    Water,
    Waterways,
    Landuse,
    Landcover,
    Transportation,
    TransportationLabels,
    Buildings,
    Pois,
    Places,
    Boundaries,
    BoundaryLabels,
    Addresses,
    Sites,
    Aerialways,
    Ferries,
    // ... remaining Shortbread layers
    Custom(u16),
}
```

## Type Aliases

```rust
/// A 2D coordinate in tile space (0..extent).
pub type Coord = (f64, f64);

/// Axis-aligned bounding box in EPSG:4326 (lon/lat).
pub type BBox = geom::Rect<f64>;

/// Feature property map.
pub type Properties = HashMap<String, Value>;
```
