//! Reserved initial content for seeded output roles. The wire bytes remain
//! `brokkr-library-seed-v1 ROLE\n`: the harness checks this recipe.
//! Reads refuse symlinks and special files. Use only after quiescence.

use std::io::{Read as _, Write as _};
use std::path::Path;

use super::describe::RoleKind;

/// The seed recipe shared with the harness.
const RECIPE: &str = "seed-v1";

/// The file a directory role's seed holds.
const DIR_ENTRY: &str = "seed";

/// The seed's bytes for `role`: a version tag and the role id, so a
/// restore that put one member's seed in another's place is visible.
pub(crate) fn bytes(role: &str) -> Vec<u8> {
    format!("brokkr-library-{RECIPE} {role}\n").into_bytes()
}

/// The seed recipe of a role of `kind`, for the verification identity.
pub(crate) fn recipe(kind: RoleKind) -> String {
    format!("{RECIPE}:{}", kind.name())
}

/// Write `role`'s seed at `path`, which must not exist: a regular file for
/// a `file` or `pbf` role, a directory holding the one file `seed` for a
/// `directory` role.
pub(crate) fn write(path: &Path, kind: RoleKind, role: &str) -> std::io::Result<()> {
    let file_at = |at: &Path| -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(at)?;
        file.write_all(&bytes(role))
    };
    match kind {
        RoleKind::Pbf | RoleKind::File => file_at(path),
        RoleKind::Directory => {
            std::fs::create_dir(path)?;
            file_at(&path.join(DIR_ENTRY))
        }
    }
}

/// What a role's destination holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum State {
    Absent,
    /// Exactly the role's seed.
    Seed,
    /// Something of the role's kind that is not its seed.
    Other,
}

impl State {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Absent => "absent",
            Self::Seed => "its seed",
            Self::Other => "present",
        }
    }
}

/// The destination at `path` of `role`, a role of `kind`, never followed
/// through a link. An entry not of the role's kind is an error.
pub(crate) fn state(path: &Path, kind: RoleKind, role: &str) -> Result<State, String> {
    let shown = path.display();
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(State::Absent),
        Err(e) => return Err(format!("{shown}: cannot inspect: {e}")),
    };
    let seed = bytes(role);
    match kind {
        RoleKind::Pbf | RoleKind::File if meta.file_type().is_file() => {
            Ok(if holds(path, &seed)? {
                State::Seed
            } else {
                State::Other
            })
        }
        RoleKind::Directory if meta.file_type().is_dir() => {
            let mut entries = Vec::new();
            for entry in
                std::fs::read_dir(path).map_err(|e| format!("{shown}: cannot list: {e}"))?
            {
                let entry = entry.map_err(|e| format!("{shown}: cannot list: {e}"))?;
                let file_type = entry
                    .file_type()
                    .map_err(|e| format!("{shown}: cannot inspect: {e}"))?;
                entries.push((entry.file_name(), file_type));
                if entries.len() > 1 {
                    return Ok(State::Other);
                }
            }
            let only_seed = matches!(
                entries.as_slice(),
                [(name, file_type)] if name.as_os_str() == DIR_ENTRY && file_type.is_file()
            );
            if only_seed && holds(&path.join(DIR_ENTRY), &seed)? {
                Ok(State::Seed)
            } else {
                Ok(State::Other)
            }
        }
        _ => Err(format!("{shown} is not a {}", kind.name())),
    }
}

/// Whether the regular file at `path` holds exactly `expected`.
fn holds(path: &Path, expected: &[u8]) -> Result<bool, String> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let shown = path.display();
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|e| format!("{shown}: cannot open: {e}"))?;
    let meta = file
        .metadata()
        .map_err(|e| format!("{shown}: cannot inspect: {e}"))?;
    if !meta.is_file() {
        return Err(format!("{shown}: not a regular file"));
    }
    if meta.len() != expected.len() as u64 {
        return Ok(false);
    }
    let mut found = Vec::with_capacity(expected.len() + 1);
    file.take(expected.len() as u64 + 1)
        .read_to_end(&mut found)
        .map_err(|e| format!("{shown}: cannot read: {e}"))?;
    Ok(found == expected)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn scratch() -> pbfhogg_test_scratch::TestScratch {
        pbfhogg_test_scratch::scratch()
    }

    #[test]
    fn a_written_seed_reads_back_as_the_seed_and_another_role_s_does_not() {
        let scratch = scratch();
        let dir = scratch.path();
        for kind in [RoleKind::Pbf, RoleKind::File, RoleKind::Directory] {
            let path = dir.join(kind.name());
            assert_eq!(state(&path, kind, "out").unwrap(), State::Absent);
            write(&path, kind, "out").unwrap();
            assert_eq!(state(&path, kind, "out").unwrap(), State::Seed);
            assert_eq!(state(&path, kind, "other").unwrap(), State::Other);
        }
    }

    #[test]
    fn changed_content_or_an_extra_entry_is_no_longer_the_seed() {
        let scratch = scratch();
        let dir = scratch.path();
        let file = dir.join("f");
        write(&file, RoleKind::File, "f").unwrap();
        std::fs::write(&file, b"brokkr-library-seed-v1 g\n").unwrap();
        assert_eq!(state(&file, RoleKind::File, "f").unwrap(), State::Other);
        let tree = dir.join("d");
        write(&tree, RoleKind::Directory, "d").unwrap();
        std::fs::write(tree.join("more"), b"x").unwrap();
        assert_eq!(
            state(&tree, RoleKind::Directory, "d").unwrap(),
            State::Other
        );
    }

    #[test]
    fn an_entry_of_the_wrong_kind_or_a_link_is_an_error() {
        let scratch = scratch();
        let dir = scratch.path();
        let file = dir.join("f");
        write(&file, RoleKind::File, "f").unwrap();
        assert!(state(&file, RoleKind::Directory, "f").is_err());
        let link = dir.join("l");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        assert!(state(&link, RoleKind::File, "f").is_err());
    }

    #[test]
    fn seed_bytes_are_the_protocol_recipe_and_existing_entries_are_never_overwritten() {
        assert_eq!(bytes("out"), b"brokkr-library-seed-v1 out\n");
        let scratch = scratch();
        for kind in [RoleKind::Pbf, RoleKind::File, RoleKind::Directory] {
            let path = scratch.path().join(kind.name());
            write(&path, kind, "first").unwrap();
            assert!(write(&path, kind, "second").is_err());
            assert_eq!(state(&path, kind, "first").unwrap(), State::Seed);
        }
    }
}
