# Installation

## CLI

Install the `pbfhogg` binary from crates.io:

```sh
cargo install pbfhogg-cli
```

This builds from source with fat LTO. The binary is called `pbfhogg`. Cargo refuses toolchains older than the `rust-version` in the workspace `Cargo.toml` (the README badge). That value is an admission floor, not a verified minimum supported Rust version: the project builds and tests on a current nightly, and no check builds on that version, so a stable toolchain at the floor may still fail to compile it.

## Library

Add the library to your `Cargo.toml`:

```toml
[dependencies]
pbfhogg = "0.6"
```

This enables the default `commands` feature. If you only need read/write (no `check`, `export`, `extract` or geocode index builder), disable it to skip `serde_json` and `s2` dependencies:

```toml
[dependencies]
pbfhogg = { version = "0.6", default-features = false }
```

### Feature flags

| Feature | Default | Description |
|---------|---------|-------------|
| `commands` | yes | Enables `check`, `export`, `extract`, the spatial helpers, JSON output for `inspect`, the geocode index builder, and their deps (`serde_json`, `s2`) |
| `geocode-reader` | included by `commands` | Enables `geocode_index::Reader` for reverse geocoding queries (depends on `s2`) |
| `linux-direct-io` | no | O_DIRECT read/write paths - bypasses page cache for planet-scale I/O (Linux only) |
| `linux-io-uring` | no | io_uring writer with registered buffers (Linux only, requires kernel 5.1+) |

The two Linux features gate only `O_DIRECT` and `io_uring`. Every Linux build, with or without them, copies passthrough blobs with `copy_file_range`, hints sequential readahead on buffered reads, and drops `add-locations-to-ways --index-type external` scratch from the page cache once it is read.

For reverse geocoding queries without the full `commands` feature:

```toml
[dependencies]
pbfhogg = { version = "0.6", default-features = false, features = ["geocode-reader"] }
```

## Building from source

```sh
git clone https://github.com/folknor/pbfhogg
cd pbfhogg
cargo install --path crates/cli
```

To enable O_DIRECT and io_uring support:

```sh
cargo install --path crates/cli --features linux-direct-io,linux-io-uring
```

### Build requirements

- Rust at or above the workspace `rust-version` in `Cargo.toml` (an unverified floor; see above)
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
