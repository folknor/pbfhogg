# Installation

## CLI

Install the `pbfhogg` binary from crates.io:

```sh
cargo install pbfhogg-cli
```

This builds from source with fat LTO. The binary is called `pbfhogg`. Requires Rust 1.87+.

## Library

Add the library to your `Cargo.toml`:

```toml
[dependencies]
pbfhogg = "0.2"
```

This enables the default `commands` feature. If you only need read/write (no extract, check-refs, or geocode index), disable it to skip `serde_json`, `roaring`, and `s2` dependencies:

```toml
[dependencies]
pbfhogg = { version = "0.2", default-features = false }
```

### Feature flags

| Feature | Default | Description |
|---------|---------|-------------|
| `commands` | yes | Enables `check_refs`, `extract`, geocode index builder, and their deps (`roaring`, `serde_json`, `s2`) |
| `geocode-reader` | included by `commands` | Enables `geocode_index::Reader` for reverse geocoding queries (depends on `s2`) |
| `linux-direct-io` | no | O_DIRECT read/write paths - bypasses page cache for planet-scale I/O (Linux only) |
| `linux-io-uring` | no | io_uring writer with registered buffers - faster writes above ~4 GB (Linux only, requires kernel 5.1+) |

For reverse geocoding queries without the full `commands` feature:

```toml
[dependencies]
pbfhogg = { version = "0.2", default-features = false, features = ["geocode-reader"] }
```

## Building from source

```sh
git clone https://github.com/folknor/pbfhogg
cd pbfhogg
cargo install --path cli
```

To enable O_DIRECT and io_uring support:

```sh
cargo install --path cli --features linux-direct-io,linux-io-uring
```

### Build requirements

- Rust 1.87 or later
- No C compiler required - all protobuf encoding is hand-rolled wire format, and zlib uses `zlib-rs` (pure Rust)
- No `protoc` or Protocol Buffers toolchain needed

## Platform

pbfhogg is developed on Linux and untested elsewhere. The core read/write functionality has no platform-specific code, but the production-relevant features (`linux-direct-io`, `linux-io-uring`) are Linux-only. O_DIRECT requires a real filesystem (not tmpfs). io_uring requires Linux 5.1+ and sufficient `RLIMIT_MEMLOCK`.

## Verifying the installation

```sh
pbfhogg --version

pbfhogg --help
```

To check that a PBF file can be read:

```sh
pbfhogg inspect denmark.osm.pbf
```
