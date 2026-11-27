//! Single-flight pbfhogg producers. The verifier has one launcher thread,
//! so only members of the same unsettled group can request a key in flight.
//! Keep those reservations in the group and publish only after settlement.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::error::Error;
use crate::process::Ended;
use crate::run::{Source, Stamp};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum Arg {
    Literal(OsString),
    Input(Source),
    Output(OsString),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum IoMode {
    Buffered,
    Direct,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct Key {
    pub(crate) executable: String,
    pub(crate) args: Vec<Arg>,
    pub(crate) io: IoMode,
    pub(crate) scratch: bool,
}

pub(crate) struct Request {
    pub(crate) key: Key,
    pub(crate) artifact: PathBuf,
}

struct Produced {
    artifact: PathBuf,
    stamp: Option<Stamp>,
    ended: Ended,
    builder: String,
}

impl Produced {
    fn capture(artifact: PathBuf, ended: Ended, builder: &str) -> Result<Self, Error> {
        let stamp = match artifact.symlink_metadata() {
            Ok(meta) => meta.is_file().then(|| Stamp::of(&meta)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(Error::tool(format!("{}: {e}", artifact.display()))),
        };
        Ok(Self {
            artifact,
            stamp,
            ended,
            builder: builder.to_owned(),
        })
    }

    fn replay(&self, target: &Path) -> Result<Outcome, Error> {
        if let Some(original) = self.stamp {
            if !self
                .artifact
                .symlink_metadata()
                .is_ok_and(|meta| meta.is_file() && Stamp::of(&meta) == original)
            {
                return Err(Error::Environment(format!(
                    "shared producer artifact {} changed during the run",
                    self.artifact.display()
                )));
            }
            if target != self.artifact.as_path() {
                std::fs::hard_link(&self.artifact, target).map_err(|e| {
                    Error::tool(format!(
                        "link shared producer artifact {} to {}: {e}",
                        self.artifact.display(),
                        target.display()
                    ))
                })?;
            }
        }
        Ok(Outcome {
            ended: self.ended.clone(),
            reuse: Some(Reuse {
                artifact: self.artifact.clone(),
                builder: self.builder.clone(),
            }),
        })
    }
}

#[derive(Default)]
pub(crate) struct Producers {
    entries: HashMap<Key, Produced>,
}

enum Slot {
    Launch,
    Produce(Request),
    Replay(Request),
}

pub(crate) struct Outcome {
    pub(crate) ended: Ended,
    pub(crate) reuse: Option<Reuse>,
}

pub(crate) struct Reuse {
    pub(crate) artifact: PathBuf,
    pub(crate) builder: String,
}

#[derive(Default)]
pub(crate) struct Batch {
    slots: Vec<Slot>,
    pending: HashSet<Key>,
}

impl Batch {
    /// Reserve a result slot in caller order. True means this caller must
    /// supply an exit from the launch group to fill the slot.
    pub(crate) fn reserve(&mut self, request: Option<Request>, producers: &Producers) -> bool {
        let slot = match request {
            None => Slot::Launch,
            Some(request) => {
                if producers.entries.contains_key(&request.key)
                    || !self.pending.insert(request.key.clone())
                {
                    Slot::Replay(request)
                } else {
                    Slot::Produce(request)
                }
            }
        };
        let launch = !matches!(&slot, Slot::Replay(_));
        self.slots.push(slot);
        launch
    }

    /// Fill the result slots after the launch group settles. A producer's
    /// exit is data, including nonzero exits and their original stderr.
    pub(crate) fn settled(
        self,
        producers: &mut Producers,
        builder: &str,
        ended: Vec<Ended>,
    ) -> Result<Vec<Outcome>, Error> {
        let mut exits = ended.into_iter();
        let mut next = || {
            exits
                .next()
                .ok_or_else(|| Error::tool("a pbfhogg launch group ended short"))
        };
        self.slots
            .into_iter()
            .map(|slot| {
                let ended = match slot {
                    Slot::Launch => next()?,
                    Slot::Produce(request) => {
                        let ended = next()?;
                        let record = Produced::capture(request.artifact, ended.clone(), builder)?;
                        producers.entries.insert(request.key, record);
                        ended
                    }
                    Slot::Replay(request) => {
                        let record = producers.entries.get(&request.key).ok_or_else(|| {
                            Error::tool("a reserved pbfhogg producer has no settled exit")
                        })?;
                        return record.replay(&request.artifact);
                    }
                };
                Ok(Outcome { ended, reuse: None })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::run::stamp;
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;

    fn request(path: &Path) -> Option<Request> {
        Some(Request {
            key: Key {
                executable: "digest".to_owned(),
                args: vec![Arg::Literal("check".into())],
                io: IoMode::Buffered,
                scratch: false,
            },
            artifact: path.to_path_buf(),
        })
    }

    fn ended(code: u8, stderr: &[u8]) -> Ended {
        Ended {
            status: ExitStatus::from_raw(i32::from(code) << 8),
            stderr: stderr.to_vec(),
        }
    }

    #[test]
    fn one_launch_fills_all_slots_for_the_same_key() {
        let scratch = pbfhogg_test_scratch::scratch();
        let original = scratch.path().join("original.json");
        let sibling = scratch.path().join("sibling.json");
        let later = scratch.path().join("later.json");
        let mut producers = Producers::default();
        let mut batch = Batch::default();
        assert!(batch.reserve(request(&original), &producers));
        assert!(batch.reserve(None, &producers));
        assert!(!batch.reserve(request(&sibling), &producers));
        assert!(!sibling.exists());
        std::fs::write(&original, b"report").unwrap();
        let outcomes = batch
            .settled(
                &mut producers,
                "repack",
                vec![ended(1, b"shared"), ended(0, b"other")],
            )
            .unwrap();
        assert_eq!(
            outcomes
                .iter()
                .map(|out| out.ended.code())
                .collect::<Vec<_>>(),
            [Some(1), Some(0), Some(1)]
        );
        assert!(outcomes[0].reuse.is_none());
        assert_eq!(outcomes[1].ended.stderr, b"other");
        assert_eq!(outcomes[2].ended.stderr, b"shared");
        assert_eq!(stamp(&original).unwrap(), stamp(&sibling).unwrap());

        let mut batch = Batch::default();
        assert!(!batch.reserve(request(&later), &producers));
        let replay = batch
            .settled(&mut producers, "degrade", Vec::new())
            .unwrap();
        let reuse = replay[0].reuse.as_ref().unwrap();
        assert_eq!(reuse.builder, "repack");
        assert_eq!(reuse.artifact, original);
        assert_eq!(std::fs::read(&later).unwrap(), b"report");
        assert_eq!(replay[0].ended.code(), Some(1));
        assert_eq!(replay[0].ended.stderr, b"shared");
        assert!(Batch::default().reserve(request(&later), &Producers::default()));
    }

    #[test]
    fn every_consumer_gets_the_failed_exit_and_its_stderr() {
        let scratch = pbfhogg_test_scratch::scratch();
        let mut producers = Producers::default();
        let mut batch = Batch::default();
        let failure = b"error from the producer\xff\n";
        for check in ["apply-changes", "diff", "diff-osc"] {
            let path = scratch.path().join(check);
            let launch = batch.reserve(request(&path), &producers);
            assert_eq!(launch, check == "apply-changes");
            let exits = if launch {
                vec![ended(2, failure)]
            } else {
                Vec::new()
            };
            let result = batch.settled(&mut producers, check, exits).unwrap();
            assert!(!result[0].ended.status.success());
            assert_eq!(result[0].ended.code(), Some(2));
            assert_eq!(result[0].ended.stderr, failure);
            assert!(!path.exists());
            batch = Batch::default();
        }
    }

    #[test]
    fn an_aborted_group_leaves_no_reusable_reservation() {
        let scratch = pbfhogg_test_scratch::scratch();
        let path = scratch.path().join("out");
        let producers = Producers::default();
        let mut aborted = Batch::default();
        assert!(aborted.reserve(request(&path), &producers));
        drop(aborted);
        assert!(Batch::default().reserve(request(&path), &producers));
    }

    #[test]
    fn a_mutating_consumer_keeps_its_own_file() {
        let scratch = pbfhogg_test_scratch::scratch();
        let original = scratch.path().join("shared.osm.pbf");
        let private = scratch.path().join("private.osm.pbf");
        let later = scratch.path().join("later.osm.pbf");
        let mut producers = Producers::default();
        let mut batch = Batch::default();
        assert!(batch.reserve(request(&original), &producers));
        // Leaving the writer unmarked gives it a fresh launch and output.
        assert!(batch.reserve(None, &producers));
        for path in [&original, &private] {
            std::fs::write(path, b"fixture").unwrap();
        }
        batch
            .settled(
                &mut producers,
                "builder",
                vec![ended(0, b""), ended(0, b"")],
            )
            .unwrap();
        std::fs::write(&private, b"edited fixture").unwrap();

        let mut batch = Batch::default();
        assert!(!batch.reserve(request(&later), &producers));
        batch.settled(&mut producers, "reader", Vec::new()).unwrap();
        assert_eq!(std::fs::read(&later).unwrap(), b"fixture");
        assert_eq!(std::fs::read(&private).unwrap(), b"edited fixture");
        assert_eq!(stamp(&original).unwrap(), stamp(&later).unwrap());
    }
}
