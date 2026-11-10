//! Command-level reach of direct I/O. Keep ignored library parameters as
//! plumbing for future transports. The CLI help and reference matrix use
//! this table; mode-specific refusals remain with option validation.

pub(super) struct Reach {
    pub path: &'static [&'static str],
    pub reads: &'static str,
    pub writes: &'static str,
    #[cfg_attr(
        not(test),
        allow(dead_code, reason = "reference matrix generation is tested")
    )]
    pub uring: &'static str,
}

pub(super) const REACH: &[Reach] = &[
    Reach {
        path: &["inspect"],
        reads: "Index scan buffered; header probes, --show and full-decode fallback direct; --nodes front probe direct with buffered classify preads",
        writes: "-",
        uring: "-",
    },
    Reach {
        path: &["inspect", "tags"],
        reads: "Kind-restricting type/expression probe direct; unfiltered counting, schedule and classify preads buffered",
        writes: "-",
        uring: "-",
    },
    Reach {
        path: &["check"],
        reads: "ID front read and index check direct; refs, locations and ID body preads buffered",
        writes: "-",
        uring: "-",
    },
    Reach {
        path: &["cat"],
        reads: "Front reads, passthrough and dedupe direct; clean classify preads buffered",
        writes: "All PBF writes direct",
        uring: "Dedupe only",
    },
    Reach {
        path: &["sort"],
        reads: "All reads direct, including copied ranges",
        writes: "All PBF writes direct",
        uring: "Yes",
    },
    Reach {
        path: &["repack"],
        reads: "Front read direct; classify preads buffered",
        writes: "All PBF writes direct",
        uring: "Yes",
    },
    Reach {
        path: &["degrade"],
        reads: "Front read and raw frames direct; decode classify preads buffered",
        writes: "All PBF writes direct",
        uring: "Yes",
    },
    Reach {
        path: &["renumber"],
        reads: "Header read direct; bulk reads buffered",
        writes: "All PBF writes direct",
        uring: "-",
    },
    Reach {
        path: &["extract"],
        reads: "Front reads direct; pread passes buffered; fallback streams direct",
        writes: "Single output direct; config single-pass outputs buffered",
        uring: "-",
    },
    Reach {
        path: &["tags-filter"],
        reads: "Front read and -R stream direct; two-pass preads and dependency scans buffered",
        writes: "All PBF writes direct; OSC refuses flag",
        uring: "-",
    },
    Reach {
        path: &["export"],
        reads: "No direct I/O flag",
        writes: "No PBF output",
        uring: "-",
    },
    Reach {
        path: &["getid"],
        reads: "Front reads and sequential fallback direct; scheduled and ID-file preads buffered",
        writes: "All PBF writes direct",
        uring: "-",
    },
    Reach {
        path: &["getparents"],
        reads: "Dispatch probe and fallback stream direct; scheduled preads buffered",
        writes: "All PBF writes direct",
        uring: "-",
    },
    Reach {
        path: &["time-filter"],
        reads: "Header and history stream direct; snapshot classify preads buffered",
        writes: "All PBF writes direct",
        uring: "-",
    },
    Reach {
        path: &["add-locations-to-ways"],
        reads: "Front reads direct; index, dependency and external-stage preads buffered",
        writes: "All PBF writes direct",
        uring: "-",
    },
    Reach {
        path: &["apply-changes"],
        reads: "Front read direct; scanner, body preads and passthrough reads buffered",
        writes: "All PBF writes direct",
        uring: "Yes",
    },
    Reach {
        path: &["diff"],
        reads: "All PBF reads direct, including indexed parallel comparison",
        writes: "No PBF output",
        uring: "-",
    },
    Reach {
        path: &["digest"],
        reads: "All reads direct",
        writes: "-",
        uring: "-",
    },
    Reach {
        path: &["build-geocode-index"],
        reads: "All PBF scans direct",
        writes: "Index output buffered, not PBF",
        uring: "-",
    },
    Reach {
        path: &["merge-changes"],
        reads: "OSC only; no direct I/O flag",
        writes: "No PBF output",
        uring: "-",
    },
];

impl Reach {
    pub fn help(&self) -> String {
        format!(
            "Use O_DIRECT where supported (requires linux-direct-io). Reads: {}. Writes: {}. Other paths use the page cache.",
            self.reads, self.writes
        )
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
fn matrix() -> String {
    let mut out = String::from(
        "| Command | Direct reads | Direct writes | `--io-uring` |\n|---------|--------------|---------------|--------------|\n",
    );
    for reach in REACH {
        use std::fmt::Write;
        writeln!(
            out,
            "| {} | {} | {} | {} |",
            reach.path.join(" "),
            reach.reads,
            reach.writes,
            reach.uring
        )
        .unwrap();
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn direct_io_matrix_matches_reach() {
        let docs = include_str!("../../docs/cli/commands.md");
        let actual = docs
            .split("<!-- io-reach:start -->\n")
            .nth(1)
            .unwrap()
            .split("<!-- io-reach:end -->")
            .next()
            .unwrap();
        assert_eq!(
            actual,
            matrix(),
            "Regenerate the I/O matrix from io_reach::matrix:\n{}",
            matrix()
        );
    }

    #[test]
    fn direct_io_help_and_feature_refusal_cover_every_command() {
        let root = crate::cli_command();
        let mut seen = Vec::new();
        fn walk(cmd: &clap::Command, path: &[String], seen: &mut Vec<Vec<String>>) {
            if cmd.get_arguments().any(|arg| arg.get_id() == "direct_io") {
                seen.push(path.to_vec());
            }
            for sub in cmd.get_subcommands() {
                let mut next = path.to_vec();
                next.push(sub.get_name().to_owned());
                walk(sub, &next, seen);
            }
        }
        for sub in root.get_subcommands() {
            walk(sub, &[sub.get_name().to_owned()], &mut seen);
        }
        for path in &seen {
            let reach = REACH
                .iter()
                .find(|r| r.path == path.as_slice())
                .expect("missing reach row");
            let mut cmd = &root;
            for part in path {
                cmd = cmd.find_subcommand(part).unwrap();
            }
            let arg = cmd
                .get_arguments()
                .find(|a| a.get_id() == "direct_io")
                .unwrap();
            assert_eq!(arg.get_help().unwrap().to_string(), reach.help());
            if !cfg!(feature = "linux-direct-io") {
                // The feature parser must win even before required input
                // arguments are supplied. This path never reaches execution.
                let mut args = vec!["pbfhogg".to_owned()];
                args.extend(path.iter().cloned());
                args.push("--direct-io".to_owned());
                let err = root.clone().try_get_matches_from(args).unwrap_err();
                assert!(
                    err.to_string().contains("linux-direct-io"),
                    "{path:?}: {err}"
                );
            }
        }
        for reach in REACH {
            if !reach
                .reads
                .to_ascii_lowercase()
                .contains("no direct i/o flag")
            {
                assert!(
                    seen.iter().any(|p| p.as_slice() == reach.path),
                    "stale reach row: {:?}",
                    reach.path
                );
            }
        }
    }
}
