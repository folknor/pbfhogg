# Installation

## Pre-built Binaries

Download the latest release for your platform from [GitHub Releases](https://github.com/user/nidhogg/releases):

| Platform | Architecture | Download |
|---|---|---|
| Linux | x86_64 | `nidhogg-x86_64-unknown-linux-gnu.tar.gz` |
| Linux | aarch64 | `nidhogg-aarch64-unknown-linux-gnu.tar.gz` |
| macOS | Apple Silicon | `nidhogg-aarch64-apple-darwin.tar.gz` |
| macOS | Intel | `nidhogg-x86_64-apple-darwin.tar.gz` |
| Windows | x86_64 | `nidhogg-x86_64-pc-windows-msvc.zip` |

```sh
# Example: Linux x86_64
curl -LO https://github.com/user/nidhogg/releases/latest/download/nidhogg-x86_64-unknown-linux-gnu.tar.gz
tar xzf nidhogg-x86_64-unknown-linux-gnu.tar.gz
sudo mv nidhogg /usr/local/bin/
```

## From Crates.io

If you have Rust installed:

```sh
cargo install nidhogg
```

This builds from source with all optimizations. Requires Rust 1.75+.

## From Source

```sh
git clone https://github.com/user/nidhogg
cd nidhogg
cargo build --release
```

The binary will be at `target/release/nidhogg`. Copy it somewhere on your `$PATH`:

```sh
sudo cp target/release/nidhogg /usr/local/bin/
```

### Build Features

Nidhogg has optional compile-time features:

| Feature | Default | Description |
|---|---|---|
| `zstd` | yes | Zstandard compression support |
| `brotli` | yes | Brotli compression support |
| `lua` | no | Lua plugin support for custom layers |
| `simd` | no | SIMD-accelerated geometry operations |

To build with Lua plugin support:

```sh
cargo build --release --features lua
```

To build a minimal binary (gzip only, no plugins):

```sh
cargo build --release --no-default-features
```

## Docker

A Docker image is available for convenience, though the native binary is recommended for production builds:

```sh
docker pull ghcr.io/user/nidhogg:latest

# Mount your data directory and run
docker run --rm -v /data:/data ghcr.io/user/nidhogg:latest \
    --ocean /data/water_polygons.shp \
    /data/input.osm.pbf /data/output.pmtiles
```

## Requirements

### Runtime

- No runtime dependencies for the default build
- An ocean shapefile is optional but recommended (see [Getting Started](./))
- Disk space: 2x input file size for temporary sort files

### Building from Source

- Rust 1.75 or later
- A C compiler (for native dependencies)
- `protoc` (Protocol Buffers compiler) — usually available via your package manager:

```sh
# Ubuntu/Debian
sudo apt install protobuf-compiler

# macOS
brew install protobuf

# Arch
sudo pacman -S protobuf
```

## Verifying the Installation

After installing, verify everything works:

```sh
nidhogg --version
# nidhogg 0.4.2

nidhogg --help
# Prints the full CLI reference
```
