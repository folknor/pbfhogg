use std::path::Path;
use std::time::{Duration, Instant};

use crate::config::DriveConfig;
use crate::db::{ResultsDb, RunRow};
use crate::env::EnvInfo;
use crate::error::DevError;
use crate::git::GitInfo;
use crate::lockfile::LockGuard;
use crate::output;

// ---------------------------------------------------------------------------
// Configuration and result types
// ---------------------------------------------------------------------------

/// Configuration for a benchmark run.
pub struct BenchConfig {
    pub command: String,
    pub variant: Option<String>,
    pub input_file: Option<String>,
    pub input_mb: Option<f64>,
    pub cargo_features: Option<String>,
    pub cargo_profile: String,
    pub runs: usize,
}

/// Result of a single benchmark measurement.
pub struct BenchResult {
    pub elapsed_ms: i64,
    pub extra: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// The benchmark harness. Holds lockfile guard, database, env snapshot, git info.
pub struct BenchHarness {
    _lock: LockGuard,
    db: ResultsDb,
    env: EnvInfo,
    git: GitInfo,
    storage_notes: Option<String>,
}

impl BenchHarness {
    /// Create a new harness, acquiring the lockfile and collecting environment.
    pub fn new(
        config: &crate::config::DevConfig,
        paths: &crate::config::ResolvedPaths,
        workspace_root: &Path,
    ) -> Result<Self, DevError> {
        let lock = crate::lockfile::acquire(&paths.scratch_dir)?;
        let env = crate::env::collect(config, paths);
        let git = crate::git::collect(workspace_root)?;
        let db = ResultsDb::open(&workspace_root.join("dev/results.db"))?;
        let storage_notes = format_storage_notes(&paths.drives);

        if !git.is_clean {
            output::bench_msg(
                "dirty tree — results go to stdout only, not stored in database",
            );
        }

        Ok(Self {
            _lock: lock,
            db,
            env,
            git,
            storage_notes,
        })
    }

    /// Internal timing: closure called N times, returns `BenchResult`.
    /// Best-of-N (minimum `elapsed_ms`).
    pub fn run_internal<F>(
        &self,
        config: &BenchConfig,
        f: F,
    ) -> Result<BenchResult, DevError>
    where
        F: Fn(usize) -> Result<BenchResult, DevError>,
    {
        let mut best: Option<BenchResult> = None;

        for i in 0..config.runs {
            output::bench_msg(&format!("run {}/{}", i + 1, config.runs));
            let result = f(i)?;
            best = Some(pick_best(best, result));
        }

        let best = best.ok_or_else(|| {
            DevError::Config("benchmark requires at least 1 run".into())
        })?;

        self.record_result(config, &best)?;
        Ok(best)
    }

    /// External timing: run subprocess N times, measure wall-clock.
    /// Best-of-N (minimum).
    pub fn run_external(
        &self,
        config: &BenchConfig,
        program: &Path,
        args: &[&str],
        cwd: &Path,
    ) -> Result<BenchResult, DevError> {
        let mut best_ms: Option<i64> = None;

        for i in 0..config.runs {
            output::bench_msg(&format!("run {}/{}", i + 1, config.runs));

            let start = Instant::now();
            let captured = output::run_captured(
                &program.display().to_string(),
                args,
                cwd,
            )?;
            let ms = elapsed_to_ms(&start.elapsed());

            if !captured.status.success() {
                let stderr = String::from_utf8_lossy(&captured.stderr);
                return Err(DevError::Subprocess {
                    program: program.display().to_string(),
                    code: captured.status.code(),
                    stderr: stderr.into_owned(),
                });
            }

            best_ms = Some(pick_best_ms(best_ms, ms));
        }

        let elapsed_ms = best_ms.ok_or_else(|| {
            DevError::Config("benchmark requires at least 1 run".into())
        })?;

        let result = BenchResult {
            elapsed_ms,
            extra: None,
        };
        self.record_result(config, &result)?;
        Ok(result)
    }

    /// Distribution timing: collect all N samples, compute min/p50/p95/max.
    #[allow(dead_code)]
    pub fn run_distribution<F>(
        &self,
        config: &BenchConfig,
        f: F,
    ) -> Result<BenchResult, DevError>
    where
        F: Fn(usize) -> Result<i64, DevError>,
    {
        let mut samples = Vec::with_capacity(config.runs);

        for i in 0..config.runs {
            output::bench_msg(&format!("run {}/{}", i + 1, config.runs));
            let ms = f(i)?;
            samples.push(ms);
        }

        samples.sort_unstable();

        let min = percentile(&samples, 0);
        let p50 = percentile(&samples, 50);
        let p95 = percentile(&samples, 95);
        let max = percentile(&samples, 100);

        let extra = serde_json::json!({
            "min_ms": min,
            "p50_ms": p50,
            "p95_ms": p95,
            "max_ms": max,
            "samples": samples.len(),
        });

        let result = BenchResult {
            elapsed_ms: min,
            extra: Some(extra),
        };

        self.record_result(config, &result)?;
        Ok(result)
    }

    // -----------------------------------------------------------------------
    // Private methods
    // -----------------------------------------------------------------------

    /// Record a result: always emit to stdout, store in DB if tree is clean.
    fn record_result(
        &self,
        config: &BenchConfig,
        result: &BenchResult,
    ) -> Result<(), DevError> {
        emit_result_lines(config, result, &self.git);

        if self.git.is_clean {
            let row = self.build_row(config, result);
            self.db.insert(&row)?;
            output::bench_msg("stored in results.db");
        }

        Ok(())
    }

    /// Build a `RunRow` from harness state, config, and result.
    fn build_row(&self, config: &BenchConfig, result: &BenchResult) -> RunRow {
        RunRow {
            hostname: self.env.hostname.clone(),
            commit: self.git.commit.clone(),
            subject: self.git.subject.clone(),
            command: config.command.clone(),
            variant: config.variant.clone(),
            input_file: config.input_file.clone(),
            input_mb: config.input_mb,
            cargo_features: config.cargo_features.clone(),
            cargo_profile: config.cargo_profile.clone(),
            elapsed_ms: result.elapsed_ms,
            kernel: Some(self.env.kernel.clone()),
            cpu_governor: Some(self.env.governor.clone()),
            avail_memory_mb: i64::try_from(self.env.memory_available_mb).ok(),
            storage_notes: self.storage_notes.clone(),
            extra: result.extra.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Free functions
// ---------------------------------------------------------------------------

/// Emit a single `[result]` line with key=value pairs.
fn emit_result_lines(
    config: &BenchConfig,
    result: &BenchResult,
    git: &GitInfo,
) {
    let mut parts = Vec::with_capacity(8);
    parts.push(format!("command={}", config.command));

    if let Some(ref v) = config.variant {
        parts.push(format!("variant={v}"));
    }

    parts.push(format!("elapsed_ms={}", result.elapsed_ms));
    parts.push(format!("commit={}", git.commit));

    if let Some(ref input) = config.input_file {
        parts.push(format!("input={input}"));
    }

    append_extra_fields(&mut parts, &result.extra);

    output::result_msg(&parts.join("  "));
}

/// Flatten top-level keys from the extra JSON object into the result line.
fn append_extra_fields(parts: &mut Vec<String>, extra: &Option<serde_json::Value>) {
    let Some(serde_json::Value::Object(map)) = extra else {
        return;
    };

    for (key, value) in map {
        let formatted = format_json_value(value);
        parts.push(format!("{key}={formatted}"));
    }
}

/// Format a JSON value for display in a result line.
fn format_json_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Null => "null".to_owned(),
        other => other.to_string(),
    }
}

/// Convert a `Duration` to milliseconds as `i64`.
fn elapsed_to_ms(duration: &Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

/// Compute a percentile from a sorted slice.
#[allow(dead_code)]
fn percentile(sorted: &[i64], pct: usize) -> i64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = (pct * (sorted.len() - 1)) / 100;
    sorted[idx]
}

/// Pick the `BenchResult` with the smaller `elapsed_ms`.
fn pick_best(current: Option<BenchResult>, candidate: BenchResult) -> BenchResult {
    match current {
        Some(best) if best.elapsed_ms <= candidate.elapsed_ms => best,
        _ => candidate,
    }
}

/// Pick the smaller of two millisecond values.
fn pick_best_ms(current: Option<i64>, candidate: i64) -> i64 {
    match current {
        Some(best) if best <= candidate => best,
        _ => candidate,
    }
}

/// Build a storage notes string from the drive configuration.
fn format_storage_notes(drives: &Option<DriveConfig>) -> Option<String> {
    let drives = drives.as_ref()?;

    let mut parts = Vec::with_capacity(4);
    push_drive_note(&mut parts, "source", &drives.source);
    push_drive_note(&mut parts, "data", &drives.data);
    push_drive_note(&mut parts, "scratch", &drives.scratch);
    push_drive_note(&mut parts, "target", &drives.target);

    if parts.is_empty() {
        return None;
    }

    Some(parts.join(", "))
}

/// Append a "label=value" note if the drive field is present.
fn push_drive_note(parts: &mut Vec<String>, label: &str, value: &Option<String>) {
    if let Some(v) = value {
        parts.push(format!("{label}={v}"));
    }
}
