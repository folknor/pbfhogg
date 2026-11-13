//! The two children the tool starts: curl for transfers and the handed
//! pbfhogg binary for the indexed variant. Each starts from an empty
//! environment plus what this module passes, with the dataset lock as its
//! stdin (see `lock`).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::lock::DatasetLock;
use crate::tunables::{CURL_RETRY_ARGS, CURL_STALL_ARGS};

/// The external programs, resolved at the executable boundary.
pub(crate) struct Tools {
    pub(crate) curl: PathBuf,
    /// Proxy and CA settings handed to curl; nothing else of the caller's
    /// environment reaches it.
    pub(crate) curl_env: Vec<(OsString, OsString)>,
    pub(crate) pbfhogg: PathBuf,
}

fn command(program: &Path, lock: &DatasetLock, child_output: Stdio) -> Result<Command, String> {
    let mut cmd = Command::new(program);
    cmd.env_clear()
        .stdin(Stdio::from(lock.for_child()?))
        .stdout(child_output)
        .stderr(Stdio::inherit());
    Ok(cmd)
}

fn run(mut cmd: Command, what: &str) -> Result<(), String> {
    let status = cmd.status().map_err(|e| format!("{what}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{what} failed: {status}"))
    }
}

/// Child operations used by acquisition. Tests provide local file operations
/// through the same interface, without interpreters or network access.
pub(crate) trait Programs {
    fn head(&self, url: &str, lock: &DatasetLock) -> Result<Option<i64>, String>;
    fn inspect_get(&self, file: &Path, key: &str, lock: &DatasetLock) -> Result<String, String>;
    fn fetch(&self, url: &str, staged: &Path, lock: &DatasetLock) -> Result<(), String>;
    fn cat(&self, input: &Path, output: &Path, lock: &DatasetLock) -> Result<(), String>;
}

impl Programs for Tools {
    fn head(&self, url: &str, lock: &DatasetLock) -> Result<Option<i64>, String> {
        let mut cmd = command(&self.curl, lock, Stdio::piped())?;
        cmd.envs(self.curl_env.iter().map(|(k, v)| (k, v)))
            .arg("-q")
            .args(crate::tunables::CURL_HEAD_ARGS)
            .args(CURL_RETRY_ARGS)
            .args(["-fsIL", url]);
        let output = cmd.output().map_err(|e| format!("curl HEAD: {e}"))?;
        if !output.status.success() {
            return Err(format!("curl HEAD {url} failed: {}", output.status));
        }
        Ok(last_modified(&String::from_utf8_lossy(&output.stdout)))
    }

    /// Header-only lookup: one unquoted value followed by a newline.
    fn inspect_get(&self, file: &Path, key: &str, lock: &DatasetLock) -> Result<String, String> {
        let mut cmd = command(&self.pbfhogg, lock, Stdio::piped())?;
        cmd.arg("inspect").arg(file).arg("--get").arg(key);
        let output = cmd.output().map_err(|e| format!("pbfhogg inspect: {e}"))?;
        if !output.status.success() {
            return Err(format!("pbfhogg inspect {key} failed: {}", output.status));
        }
        String::from_utf8(output.stdout)
            .map(|s| s.trim().to_owned())
            .map_err(|e| format!("pbfhogg inspect {key}: {e}"))
    }

    /// Download `url` into `staged` with a progress bar on stderr. `-q` first
    /// keeps curl from reading a user `.curlrc`.
    fn fetch(&self, url: &str, staged: &Path, lock: &DatasetLock) -> Result<(), String> {
        let mut cmd = command(&self.curl, lock, Stdio::null())?;
        cmd.envs(self.curl_env.iter().map(|(k, v)| (k, v)))
            .arg("-q")
            .args(CURL_STALL_ARGS)
            .args(CURL_RETRY_ARGS)
            .args(["-fL", "--progress-bar", "-o"])
            .arg(staged)
            .arg(url);
        run(cmd, &format!("curl {url}"))
    }

    /// `pbfhogg cat INPUT -o OUTPUT`: the form pbfhogg documents for
    /// generating indexdata.
    fn cat(&self, input: &Path, output: &Path, lock: &DatasetLock) -> Result<(), String> {
        let mut cmd = command(&self.pbfhogg, lock, Stdio::null())?;
        cmd.arg("cat").arg(input).arg("-o").arg(output);
        run(cmd, &format!("pbfhogg cat {}", input.display()))
    }
}

fn parse_http_date(s: &str) -> Result<i64, String> {
    // Format: "Day, DD Mon YYYY HH:MM:SS GMT"
    // Example: "Tue, 11 Apr 2026 12:34:56 GMT"
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.len() != 6 {
        return Err(format!("expected six parts: '{s}'"));
    }
    if parts[5] != "GMT" {
        return Err(format!(
            "expected GMT timezone in '{s}', found '{}'",
            parts[5]
        ));
    }
    let day: u32 = parts[1].parse().map_err(|e| format!("day: {e}"))?;
    let month: u32 = match parts[2] {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        other => return Err(format!("unknown month: {other}")),
    };
    let year: i32 = parts[3].parse().map_err(|e| format!("year: {e}"))?;
    let time_parts: Vec<&str> = parts[4].split(':').collect();
    if time_parts.len() != 3 {
        return Err(format!("bad time: '{}'", parts[4]));
    }
    let hour: u32 = time_parts[0].parse().map_err(|e| format!("hour: {e}"))?;
    let minute: u32 = time_parts[1].parse().map_err(|e| format!("minute: {e}"))?;
    let second: u32 = time_parts[2].parse().map_err(|e| format!("second: {e}"))?;

    let date = crate::date::CivilDate::new(year, month, day)
        .ok_or_else(|| format!("invalid date: '{s}'"))?;
    date.to_unix_seconds(hour, minute, second)
        .ok_or_else(|| format!("invalid time: '{}'", parts[4]))
}

// Reset at each response, including proxy CONNECT and redirect responses.
fn last_modified(headers: &str) -> Option<i64> {
    let mut value = None;
    for line in headers.lines() {
        if line.starts_with("HTTP/") {
            value = None;
        }
        if let Some((name, date)) = line.split_once(':')
            && name.eq_ignore_ascii_case("last-modified")
        {
            value = parse_http_date(date.trim()).ok();
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_dates_and_final_response_headers() {
        assert_eq!(
            parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT").ok(),
            Some(784_111_777)
        );
        assert_eq!(
            parse_http_date("Sat, 11 Apr 2026 12:34:56 GMT").ok(),
            Some(1_775_910_896)
        );
        for value in [
            "not a date",
            "Sun, 06 XYZ 1994 08:49:37 GMT",
            "Sun, 31 Feb 2026 08:49:37 GMT",
            "Sun, 06 Nov 1994 24:00:00 GMT",
        ] {
            assert!(parse_http_date(value).is_err());
        }
        let redirect = "HTTP/1.1 302 Found\r\nLast-Modified: Sun, 06 Nov 1994 08:49:37 GMT\r\n\r\n";
        assert_eq!(
            last_modified(&format!("{redirect}HTTP/2 200\r\n\r\n")),
            None
        );
        assert_eq!(
            last_modified(&format!(
                "{redirect}HTTP/2 200\r\nlAsT-mOdIfIeD: Sat, 11 Apr 2026 12:34:56 GMT\r\n\r\n"
            )),
            Some(1_775_910_896)
        );
    }
}
