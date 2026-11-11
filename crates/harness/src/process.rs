//! The harness's own process policy (ADR-0016: process state is the
//! caller's). Applied once, before any library work, as the plan says.

/// A soft `RLIMIT_NOFILE` as the context reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Limit {
    Finite(u64),
    Unlimited,
}

impl Limit {
    fn of(raw: libc::rlim_t) -> Self {
        if raw == libc::RLIM_INFINITY {
            Self::Unlimited
        } else {
            Self::Finite(raw)
        }
    }

    pub(crate) fn to_json(self) -> serde_json::Value {
        match self {
            Self::Finite(n) => serde_json::json!(n),
            Self::Unlimited => serde_json::json!("unlimited"),
        }
    }
}

fn get() -> Result<libc::rlimit, String> {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: a valid out-pointer to one rlimit.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &raw mut limit) } != 0 {
        return Err(format!(
            "getrlimit(RLIMIT_NOFILE): {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(limit)
}

/// Raise the soft descriptor limit to the hard one, unless it is there
/// already, and read it back. An error refuses the request: the plan's
/// policy could not be established.
fn raise_nofile_to_hard() -> Result<(), String> {
    let limit = get()?;
    if limit.rlim_cur != limit.rlim_max {
        let raised = libc::rlimit {
            rlim_cur: limit.rlim_max,
            rlim_max: limit.rlim_max,
        };
        // SAFETY: a valid pointer to one rlimit.
        if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &raw const raised) } != 0 {
            return Err(format!(
                "setrlimit(RLIMIT_NOFILE): {}",
                std::io::Error::last_os_error()
            ));
        }
    }
    let after = get()?;
    if after.rlim_cur == after.rlim_max {
        Ok(())
    } else {
        Err("the soft RLIMIT_NOFILE did not reach the hard limit".into())
    }
}

/// A scenario's soft `RLIMIT_NOFILE` policy, applied once at launch.
///
/// Every scenario raises to the hard limit. Descriptor scenarios grant an
/// exact `FdPool` budget instead of lowering the process limit, so admission
/// is tested independently of inherited limits; real-limit probing belongs
/// to child-process contract tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Nofile {
    /// Raise the soft limit to the hard one.
    RaiseToHard,
}

impl Nofile {
    pub(crate) fn apply(self) -> Result<(), String> {
        match self {
            Self::RaiseToHard => raise_nofile_to_hard(),
        }
    }

    /// The plan's spelling.
    pub(crate) fn to_json(self) -> serde_json::Value {
        match self {
            Self::RaiseToHard => serde_json::json!("raise_to_hard"),
        }
    }
}

/// The soft descriptor limit now, for a call's context.
pub(crate) fn soft_nofile() -> Result<Limit, String> {
    get().map(|limit| Limit::of(limit.rlim_cur))
}
