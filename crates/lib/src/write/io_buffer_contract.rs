//! Runtime arithmetic contract for the shared I/O transfer size.
//!
//! Source ownership is audited once per check by
//! crates/source-policy/src/rules/io_sizes.rs.
//! Backend boundary and allocation tests live beside each backend.

#[test]
fn io_transfer_size_is_nonzero_and_aligned() {
    use crate::direct_io::DIO_ALIGN_FLOOR;
    use crate::tunables::IO_BUF_BYTES;

    const { assert!(IO_BUF_BYTES > 0) };
    assert!(IO_BUF_BYTES.is_multiple_of(DIO_ALIGN_FLOOR));
}

#[test]
#[cfg(any(feature = "linux-direct-io", feature = "linux-io-uring"))]
fn io_transfer_rounding_preserves_boundaries() {
    use crate::direct_io::DIO_ALIGN_FLOOR;
    use crate::tunables::IO_BUF_BYTES;

    assert_eq!(
        crate::direct_io::round_up(IO_BUF_BYTES - 1, DIO_ALIGN_FLOOR),
        IO_BUF_BYTES,
    );
    assert_eq!(
        crate::direct_io::round_up(IO_BUF_BYTES + 1, DIO_ALIGN_FLOOR),
        IO_BUF_BYTES + DIO_ALIGN_FLOOR,
    );
}
