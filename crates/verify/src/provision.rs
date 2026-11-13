//! Pinned tools installed only by the live lifecycle owner, in one rename.
use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::error::Error;
use crate::process::{Launch, Lifecycle};
use crate::reference::OsmosisTools;

const OSMOSIS_VERSION: &str = "0.49.2";
const OSMOSIS_SHA256: &str = "2a23a7bf7499a0727ae6987cf66fcafc03b37379dc02177c730f352bd67a0b97";
const JDK_RELEASE: &str = "jdk-25.0.4.1+1";
const JDK_ARCHIVE_VERSION: &str = "25.0.4.1_1";

fn jdk() -> Result<(&'static str, &'static str), Error> {
    match std::env::consts::ARCH {
        "x86_64" => Ok((
            "x64",
            "dbb698396d478e7fa2b1e50f4103324b2a99b90569ee27c33f2261f9215cf41e",
        )),
        "aarch64" => Ok((
            "aarch64",
            "69df11a02cfa3ef7d7ca645e03edce6778ec090e100f6ae2b42097865730ac52",
        )),
        _ => Err(Error::tool("no pinned Linux JDK for this architecture")),
    }
}

fn marker(checksum: &str) -> String {
    format!("osmosis {OSMOSIS_VERSION}\n{OSMOSIS_SHA256}\n{JDK_RELEASE}\n{checksum}\n")
}

pub(crate) fn sha256(path: &Path) -> Result<String, Error> {
    let mut input = std::fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = vec![0; crate::tunables::DIGEST_BUFFER];
    loop {
        let n = input.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn installed(root: &Path, checksum: &str) -> Result<OsmosisTools, Error> {
    if std::fs::read_to_string(root.join("complete"))
        .ok()
        .as_deref()
        != Some(&marker(checksum))
        || !root.join("bin/osmosis").is_file()
        || !root.join("jdk/bin/java").is_file()
    {
        return Err(Error::tool(format!(
            "incomplete or unrecognised osmosis installation {}; move it aside before retrying",
            root.display()
        )));
    }
    Ok(OsmosisTools {
        osmosis: root.join("bin/osmosis"),
        java_home: root.join("jdk"),
    })
}

pub(crate) struct Downloader {
    pub(crate) curl: PathBuf,
    pub(crate) tar: PathBuf,
    pub(crate) env: Vec<(OsString, OsString)>,
}

impl Downloader {
    fn launch(
        &self,
        life: &mut Lifecycle,
        program: &Path,
        args: Vec<OsString>,
        private: &Path,
        download: bool,
    ) -> Result<(), Error> {
        let mut env = crate::reference::run_env(private);
        if download {
            env.extend(self.env.clone());
        }
        let ended = life.run(&Launch {
            program,
            args,
            env,
            cwd: private,
            sink: &private.join("stdout"),
        })?;
        if !ended.status.success() {
            return Err(Error::tool(format!(
                "{}: {:?}: {}",
                program.display(),
                ended.status,
                ended.stderr_text()
            )));
        }
        Ok(())
    }

    pub(crate) fn fetch(
        &self,
        life: &mut Lifecycle,
        private: &Path,
        url: &str,
        dest: &Path,
        checksum: &str,
    ) -> Result<(), Error> {
        crate::output::note(&format!("downloading {url}"));
        self.launch(
            life,
            &self.curl,
            [
                "-q",
                "--connect-timeout",
                "30",
                "--speed-limit",
                "1024",
                "--speed-time",
                "60",
                "--retry",
                "3",
                "--retry-max-time",
                "30",
                "-fsSL",
                "-o",
            ]
            .into_iter()
            .map(OsString::from)
            .chain([dest.as_os_str().to_owned(), url.into()])
            .collect(),
            private,
            true,
        )?;
        if sha256(dest)? != checksum {
            return Err(Error::tool(format!("sha256 mismatch downloading {url}")));
        }
        Ok(())
    }

    pub(crate) fn ensure(
        &self,
        life: &mut Lifecycle,
        data: &Path,
        run_id: &str,
    ) -> Result<OsmosisTools, Error> {
        let (arch, checksum) = jdk()?;
        let root = data.join(format!("osmosis-{OSMOSIS_VERSION}"));
        if root.try_exists()? {
            return installed(&root, checksum);
        }
        std::fs::create_dir_all(data)?;
        let private = data.join(format!(".osmosis-{run_id}"));
        std::fs::create_dir(&private)?;
        for name in ["home", "scratch", "unpack"] {
            std::fs::create_dir(private.join(name))?;
        }
        let archive = private.join("osmosis.tar");
        self.fetch(life, &private, &format!(
            "https://github.com/openstreetmap/osmosis/releases/download/{OSMOSIS_VERSION}/osmosis-{OSMOSIS_VERSION}.tar"
        ), &archive, OSMOSIS_SHA256)?;
        self.launch(
            life,
            &self.tar,
            vec![
                "xf".into(),
                archive.into_os_string(),
                "-C".into(),
                private.join("unpack").into_os_string(),
            ],
            &private,
            false,
        )?;
        let tree = private
            .join("unpack")
            .join(format!("osmosis-{OSMOSIS_VERSION}"));
        std::fs::create_dir(tree.join("jdk"))?;
        let archive = private.join("jdk.tar.gz");
        let release = JDK_RELEASE.replace('+', "%2B");
        self.fetch(life, &private, &format!(
            "https://github.com/adoptium/temurin25-binaries/releases/download/{release}/OpenJDK25U-jdk_{arch}_linux_hotspot_{JDK_ARCHIVE_VERSION}.tar.gz"
        ), &archive, checksum)?;
        self.launch(
            life,
            &self.tar,
            vec![
                "xzf".into(),
                archive.into_os_string(),
                "-C".into(),
                tree.join("jdk").into_os_string(),
                "--strip-components=1".into(),
            ],
            &private,
            false,
        )?;
        complete(life, &tree, &root, checksum)?;
        std::fs::remove_dir_all(private)?;
        installed(&root, checksum)
    }
}

/// The only publication point, after every writer has settled.
pub(crate) fn complete(
    life: &mut Lifecycle,
    tree: &Path,
    root: &Path,
    checksum: &str,
) -> Result<(), Error> {
    std::fs::write(tree.join("complete"), marker(checksum))?;
    installed(tree, checksum)?;
    life.checkpoint()?;
    std::fs::rename(tree, root)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn provisioning_digest_and_completion_are_pinned() {
        let scratch = pbfhogg_test_scratch::scratch();
        let root = scratch.path();
        let file = root.join("abc");
        std::fs::write(&file, b"abc").unwrap();
        assert_eq!(
            sha256(&file).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert!(installed(root, "pin").is_err());
        std::fs::create_dir(root.join("bin")).unwrap();
        std::fs::create_dir_all(root.join("jdk/bin")).unwrap();
        std::fs::write(root.join("bin/osmosis"), "").unwrap();
        std::fs::write(root.join("jdk/bin/java"), "").unwrap();
        std::fs::write(root.join("complete"), marker("pin")).unwrap();
        assert!(installed(root, "pin").is_ok());
        assert!(installed(root, "other-pin").is_err());
    }
}
