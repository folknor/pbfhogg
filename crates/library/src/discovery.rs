//! Describe launch and checks, after process quiescence.
use crate::{describe, process, tunables};
use std::path::Path;

pub(crate) fn load(harness: &Path) -> Result<describe::Describe, String> {
    let mut command = process::command(harness);
    command.arg("--describe-harness");
    let result =
        process::run(command, tunables::DESCRIBE_DEADLINE, None).map_err(|e| e.to_string())?;
    judge(&result)
}

fn judge(result: &process::Completed) -> Result<describe::Describe, String> {
    if result.timed_out {
        return Err("describe exceeded its 60 second deadline".into());
    }
    if result.residual_children {
        return Err("describe left residual children (killed and reaped)".into());
    }
    if !result.status.success() {
        return Err(format!("describe exited {}", result.status));
    }
    if result.err.overflow || !result.err.bytes.is_empty() {
        return Err("describe wrote stderr".into());
    }
    if result.out.overflow {
        return Err("describe exceeds the 1 MiB capture limit".into());
    }
    describe::parse(&result.out.bytes)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    fn completed() -> process::Completed {
        process::Completed {
            elapsed: tunables::DESCRIBE_DEADLINE,
            status: std::process::ExitStatus::from_raw(0), timed_out: false, residual_children: false,
            out: process::Capture { bytes: br#"{"protocol":1,"scenarios":[{"id":"s","description":"d","deadline_s":1,"roles":[],"calls":[{"id":"a","label":"a"}],"assertions":[]}]}"#.to_vec(), overflow: false },
            err: process::Capture::default(), markers: process::Capture::default(),
        }
    }
    #[test]
    fn describe_requires_clean_completion_before_parsing() {
        assert_eq!(judge(&completed()).unwrap().scenarios[0].id, "s");
        let mut result = completed();
        result.status = std::process::ExitStatus::from_raw(2 << 8);
        assert!(judge(&result).unwrap_err().contains("exited"));
        let mut result = completed();
        result.err.bytes = b"warning".to_vec();
        assert!(judge(&result).unwrap_err().contains("stderr"));
        let mut result = completed();
        result.err.overflow = true;
        assert!(judge(&result).unwrap_err().contains("stderr"));
        let mut result = completed();
        result.out.overflow = true;
        assert!(judge(&result).unwrap_err().contains("capture limit"));
        let mut result = completed();
        result.timed_out = true;
        result.out.bytes.clear();
        assert!(judge(&result).unwrap_err().contains("deadline"));
        let mut result = completed();
        result.residual_children = true;
        result.out.bytes.clear();
        assert!(judge(&result).unwrap_err().contains("residual children"));
    }
}
