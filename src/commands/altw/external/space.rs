//! External-join scratch planning, checked once stage 1 has finished.
//!
//! The check runs between stage 1 and stage 2 rather than before stage 1.
//! Indexdata counts elements, not references, so an exact pre-stage-1 figure
//! would need an extra read and decode of every way blob - repeating stage
//! 1's own way-blob read, decompress and ref scan, a large fraction of its
//! wall, on a measured path - to save at most one stage-1 wall. A bound
//! from compressed size or element counts would refuse valid, highly
//! compressible inputs. Stage 1 already produces the exact ref count
//! (`total_slots`), and its own ID shards are on disk by then, so free
//! space reflects them. The check guards the expensive part: stages 2 to 4
//! run minutes to hours at planet, and without it they find ENOSPC
//! wherever they hit it. An ENOSPC during stage 1 surfaces as an ordinary
//! write error within that stage.

use std::path::Path;

use super::super::Result;

/// Scratch still to be written after stage 1, in bytes.
///
/// Stage 2 writes one 12-byte resolved record per ref into the slot buckets
/// while the ID shards are still live. Stage 3 writes payload files: two
/// differences of i32 coordinates take at most ten varint bytes per ref, plus
/// at most one pin-bitmap byte per ref. ID shards are removed between stage 2
/// and stage 3, but that removal is best effort, so it is not credited. The
/// sidecars and ID shards already exist and are reflected in free space. Add
/// per-file allocation slack for every slot bucket and payload file. The
/// final PBF is separate.
fn remaining_scratch_bytes(refs: u64, slot_buckets: usize, payload_files: usize) -> Result<u64> {
    let record_bytes = super::RESOLVED_ENTRY_SIZE as u64 + 11;
    refs.checked_mul(record_bytes)
        .and_then(|n| {
            let files = (slot_buckets as u64).checked_add(payload_files as u64)?;
            n.checked_add(files.checked_mul(65536)?)
        })
        .ok_or_else(|| crate::Error::internal("external join scratch-space estimate overflow"))
}

/// Refuse before stage 2 when the scratch filesystem cannot hold what stages
/// 2 and 3 will write. Reads no input and writes no scratch data.
pub(super) fn precheck(
    scratch: &Path,
    total_slots: u64,
    slot_buckets: usize,
    payload_files: usize,
) -> Result<()> {
    crate::debug::emit_marker("EXTJOIN_SPACE_PLAN_START");
    let bytes = remaining_scratch_bytes(total_slots, slot_buckets, payload_files)?;
    crate::debug::emit_counter("extjoin_scratch_needed_bytes", bytes);
    check_free_space(scratch, bytes)?;
    crate::debug::emit_marker("EXTJOIN_SPACE_PLAN_END");
    Ok(())
}

#[cfg(target_os = "linux")]
fn check_free_space(scratch: &Path, bytes: u64) -> Result<()> {
    let available = super::super::filesystem_available_bytes(scratch, "external join")?;
    require_space(scratch, bytes, available)
}

#[cfg(not(target_os = "linux"))]
fn check_free_space(_scratch: &Path, _bytes: u64) -> Result<()> {
    // Match sparse's platform scope: statvfs is available through this
    // crate's libc dependency on Linux. Write errors remain authoritative.
    Ok(())
}

fn require_space(scratch: &Path, needed: u64, available: u64) -> Result<()> {
    if available < needed {
        return Err(crate::Error::io_kind(
            std::io::ErrorKind::StorageFull,
            format!(
                "insufficient free space for external join scratch: stages 2-4 need {needed} more bytes, have {available} bytes in {} (final output space is additional)",
                scratch.display()
            ),
        ));
    }
    // This is a precheck, not a reservation against concurrent writers.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_covers_slot_records_payloads_and_file_slack() {
        let needed = remaining_scratch_bytes(100, 2, 1).expect("estimate");
        assert_eq!(needed, 100 * 23 + 3 * 65536);
        assert!(remaining_scratch_bytes(u64::MAX, 0, 0).is_err());
    }

    #[test]
    fn refusal_names_bytes_and_filesystem_path() {
        let error = require_space(Path::new("scratch-dir"), 100, 99)
            .expect_err("insufficient space")
            .to_string();
        assert!(error.contains("need 100 more bytes"));
        assert!(error.contains("have 99 bytes"));
        assert!(error.contains("scratch-dir"));
        assert!(require_space(Path::new("scratch-dir"), 100, 100).is_ok());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn statvfs_rejects_impossible_scratch_without_creating_files() {
        let dir = crate::path_guard::tests::scratch();
        let error = check_free_space(dir.path(), u64::MAX)
            .expect_err("impossible scratch requirement")
            .to_string();
        assert!(error.contains("insufficient free space for external join scratch"));
        assert_eq!(std::fs::read_dir(dir.path()).expect("scratch").count(), 0);
    }
}
