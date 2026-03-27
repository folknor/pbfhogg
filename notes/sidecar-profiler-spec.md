# Sidecar profiler spec

A monitoring sidecar that runs alongside pbfhogg processes during benchmarks.
Samples system metrics at fixed intervals and records application phase markers
as a separate event stream. Produces a structured timeline for analysis.

## Architecture

```
brokkr bench commands <cmd> --profile
  ├── creates marker FIFO in scratch dir
  ├── sidecar thread opens FIFO read end (guarantees reader exists)
  ├── spawns pbfhogg <cmd> (inherits FIFO path via env var)
  │     ├── emit_marker("STAGE1_START")  (writes timestamp + name)
  │     ├── ... work ...
  │     └── emit_marker("STAGE2_START")
  ├── sidecar samples /proc every 100ms, reads FIFO markers independently
  ├── sidecar detects child exit via child handle (not bare PID)
  ├── sidecar buffers all data in memory
  └── on child exit: bulk-insert to SQLite (zero I/O during benchmark)
```

## Startup ordering (critical)

1. brokkr creates FIFO: `mkfifo <scratch>/.sidecar-{pid}.fifo`
2. Sidecar thread opens FIFO read end (blocking open is fine — instant since
   no writer yet). This guarantees a reader exists.
3. brokkr spawns pbfhogg with `PBFHOGG_MARKER_FIFO` env var
4. pbfhogg's first `emit_marker` opens the write end with `O_NONBLOCK` — succeeds
   because the sidecar already has the read end open.

Without this ordering, the writer's `O_NONBLOCK` open gets `ENXIO` (no reader)
and the `OnceLock` caches `None` permanently, silently losing all markers.

## Sample interval

Fixed at 100ms. For short Denmark-scale runs 100ms still gives 60+ samples —
sufficient.

Use `clock_nanosleep` with `TIMER_ABSTIME` and `CLOCK_MONOTONIC` (not
CLOCK_REALTIME) to avoid drift from /proc read overhead (~30µs per tick).

## Metrics per sample

### Process metrics (from /proc/{pid}/*)

| Source | Field | What it tells us |
|--------|-------|-----------------|
| `/proc/{pid}/stat` | utime | User CPU ticks (cumulative, in clock ticks — divide by `sysconf(_SC_CLK_TCK)` for seconds) |
| `/proc/{pid}/stat` | stime | Kernel CPU ticks (cumulative, same units as utime) |
| `/proc/{pid}/stat` | num_threads | Thread count at sample time |
| `/proc/{pid}/stat` | vsize | Virtual memory size (bytes) |
| `/proc/{pid}/stat` | rss | Resident pages (× page_size for bytes) |
| `/proc/{pid}/stat` | minflt | Minor page faults — page cache hit (cumulative) |
| `/proc/{pid}/stat` | majflt | Major page faults — disk read required (cumulative) |
| `/proc/{pid}/stat` | starttime | Process start time (for wall-clock alignment) |
| `/proc/{pid}/io` | rchar | Logical bytes read via syscalls (cumulative, includes page cache) |
| `/proc/{pid}/io` | wchar | Logical bytes written via syscalls (cumulative, includes page cache) |
| `/proc/{pid}/io` | read_bytes | Actual bytes read from disk (cumulative) |
| `/proc/{pid}/io` | write_bytes | Actual bytes written to disk (cumulative) |
| `/proc/{pid}/io` | cancelled_write_bytes | Bytes cancelled (temp file create+delete) |
| `/proc/{pid}/io` | syscr | Read syscall count (cumulative) |
| `/proc/{pid}/io` | syscw | Write syscall count (cumulative) |
| `/proc/{pid}/status` | VmRSS | Resident set size (kB) |
| `/proc/{pid}/status` | RssAnon | Anonymous (heap) RSS (kB) |
| `/proc/{pid}/status` | RssFile | File-backed RSS (kB) |
| `/proc/{pid}/status` | RssShmem | Shared memory RSS (kB) — completes the accounting |
| `/proc/{pid}/status` | VmSwap | Swap usage (kB) |
| `/proc/{pid}/status` | VmHWM | Peak RSS (kB) — kernel authoritative, tracked per sample |
| `/proc/{pid}/status` | voluntary_ctxt_switches | Voluntary context switches (I/O waits) |
| `/proc/{pid}/status` | nonvoluntary_ctxt_switches | Involuntary (preempted by scheduler) |

**Note:** `/proc/{pid}/io` requires same-UID or `CAP_SYS_PTRACE`. Since brokkr
spawns pbfhogg as a child with the same UID, this is fine. Comment in code for
anyone trying to attach to an external process.

**Note on `/proc/{pid}/stat` parsing:** Field counts vary between kernel
versions. Parse by field index (space-delimited after the comm field in
parentheses), not by name. The comm field can contain spaces and
parentheses — find the last `)` to locate the end of field 2.

### Derived metrics (computed from deltas during analysis, not stored)

| Metric | Formula | Notes |
|--------|---------|-------|
| cpu_user_pct | delta(utime) / `_SC_CLK_TCK` / interval_sec / num_cpus × 100 | Convert ticks to seconds first |
| cpu_sys_pct | delta(stime) / `_SC_CLK_TCK` / interval_sec / num_cpus × 100 | Same |
| disk_read_rate | delta(read_bytes) / interval_sec | MB/s |
| disk_write_rate | delta(write_bytes) / interval_sec | MB/s |
| cache_hit_ratio | 1 - clamp(delta(read_bytes) / max(delta(rchar), 1), 0, 1) | Clamp to [0,1], guard div-by-zero |
| majflt_rate | delta(majflt) / interval_sec | Faults/sec, thrashing indicator |
| minflt_rate | delta(minflt) / interval_sec | Cache hits/sec |
| syscall_rate | (delta(syscr) + delta(syscw)) / interval_sec | Syscall pressure |

### System metrics (optional, enabled with --profile-system)

| Source | Field | What it tells us |
|--------|-------|-----------------|
| `/proc/stat` | per-CPU idle/busy | Which cores are active |
| `/proc/meminfo` | MemAvailable, Dirty, Writeback | System memory pressure |
| `/proc/pressure/cpu` | some/full avg10 | CPU pressure stall information |
| `/proc/pressure/io` | some/full avg10 | I/O pressure stall information |
| `/proc/pressure/memory` | some/full avg10 | Memory pressure stall information |
| `/proc/diskstats` | per-device read/write ios | Device-level IOPS and queue depth |

System metrics help diagnose contention from background processes (explains
noisy benchmark results). `/proc/pressure/*` requires `CONFIG_PSI` in the
kernel — gracefully degrade if files are missing.

## Marker protocol

Markers are independent timestamped events, stored in a separate table from
samples. Markers are expected to be rare phase boundaries (STAGE1_START,
PASS2_END, etc.), not frequent per-batch events.

### Timing

**pbfhogg timestamps markers before writing.** The marker format includes a
monotonic timestamp from the process side, so marker timing is not quantized
by the sidecar's 100ms sample interval:

```
1234567890123 STAGE1_START
1234567890456 STAGE1_END
```

Where the number is `CLOCK_MONOTONIC` microseconds since process start.
The sidecar stores the process-provided timestamp, not its own receipt time.

### FIFO setup

brokkr creates a FIFO in the scratch directory (not /tmp):
```
mkfifo <scratch>/.sidecar-{pid}.fifo
```

The sidecar thread opens the read end BEFORE pbfhogg is spawned.
pbfhogg receives the path via environment variable:
```
PBFHOGG_MARKER_FIFO=<scratch>/.sidecar-{pid}.fifo
```

### FIFO fd caching in pbfhogg

The pbfhogg process opens the FIFO once with `O_NONBLOCK` and caches the fd
for the lifetime of the process. The open succeeds because the sidecar
already has the read end open (see startup ordering):

```rust
use std::sync::OnceLock;
use std::fs::File;
use std::io::Write;
use std::time::Instant;

static MARKER_FILE: OnceLock<Option<(File, Instant)>> = OnceLock::new();

fn marker_state() -> Option<&'static (File, Instant)> {
    MARKER_FILE.get_or_init(|| {
        let path = std::env::var("PBFHOGG_MARKER_FIFO").ok()?;
        let f = std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&path)
            .ok()?;
        Some((f, Instant::now()))
    }).as_ref()
}

pub(crate) fn emit_marker(name: &str) {
    if let Some((f, start)) = marker_state() {
        let us = start.elapsed().as_micros();
        // Non-blocking write — silently dropped if FIFO buffer full
        let _ = (&*f).write_all(format!("{us} {name}\n").as_bytes());
    }
}
```

### Sidecar reads markers independently of sampling

The sidecar reads the FIFO on a separate polling cadence (or via
`epoll`/`select` alongside the sleep). Markers are NOT read only on the
100ms sample tick — they're drained whenever available. Each line becomes
a row in the `profile_markers` table with the process-provided timestamp.

## Data storage

### In-memory buffering during benchmark

The sidecar accumulates ALL data in memory during the run:
- `Vec<Sample>` for metric samples (~200 bytes each)
- `Vec<Marker>` for marker events (~40 bytes each)
- Last observed `VmHWM` value (updated from every /proc/{pid}/status read)

After the target process exits, bulk-insert everything to SQLite in a
single transaction. This eliminates all I/O contention during the benchmark.

30K samples × 200 bytes + ~100 markers × 40 bytes = ~6 MB peak. Trivial.

### SQLite schema

Single final schema. `run_idx` supports multi-run benchmarks (`--runs N`):

```sql
CREATE TABLE profile_samples (
    result_uuid TEXT NOT NULL,
    run_idx     INTEGER NOT NULL DEFAULT 0,
    sample_idx  INTEGER NOT NULL,
    timestamp_us INTEGER NOT NULL,    -- CLOCK_MONOTONIC µs since sidecar start

    -- Process memory
    rss_kb      INTEGER,
    anon_kb     INTEGER,
    file_kb     INTEGER,
    shmem_kb    INTEGER,
    swap_kb     INTEGER,
    vsize_kb    INTEGER,
    vm_hwm_kb   INTEGER,

    -- CPU (raw clock ticks — convert via _SC_CLK_TCK for analysis)
    utime       INTEGER,
    stime       INTEGER,
    num_threads INTEGER,

    -- Page faults (cumulative)
    minflt      INTEGER,
    majflt      INTEGER,

    -- I/O (cumulative bytes)
    rchar       INTEGER,
    wchar       INTEGER,
    read_bytes  INTEGER,
    write_bytes INTEGER,
    cancelled_write_bytes INTEGER,
    syscr       INTEGER,
    syscw       INTEGER,

    -- Context switches (cumulative)
    vol_cs      INTEGER,
    nonvol_cs   INTEGER,

    PRIMARY KEY (result_uuid, run_idx, sample_idx)
);

CREATE TABLE profile_markers (
    result_uuid  TEXT NOT NULL,
    run_idx      INTEGER NOT NULL DEFAULT 0,
    marker_idx   INTEGER NOT NULL,
    timestamp_us INTEGER NOT NULL,    -- from pbfhogg (process CLOCK_MONOTONIC µs)
    name         TEXT NOT NULL,

    PRIMARY KEY (result_uuid, run_idx, marker_idx)
);

CREATE TABLE profile_summary (
    result_uuid  TEXT NOT NULL,
    run_idx      INTEGER NOT NULL DEFAULT 0,
    vm_hwm_kb    INTEGER,             -- last observed peak RSS
    sample_count INTEGER,
    marker_count INTEGER,
    wall_time_ms INTEGER,

    PRIMARY KEY (result_uuid, run_idx)
);
```

Stored in `.brokkr/results.db` alongside existing benchmark results.

### CSV export

```
brokkr results <uuid> --timeline > timeline.csv
brokkr results <uuid> --markers > markers.csv
```

## brokkr integration

### Invocation

```
brokkr bench commands <cmd> --profile [--profile-system]
```

`--profile` is a field on `ModeArgs` so every measured command gets it for
free. `run_measured` sets up the sidecar when the flag is present. Without
it, zero overhead. Results are stored in SQLite alongside the benchmark result.

### Child lifetime tracking

The sidecar detects child exit via the child process handle (e.g.,
`Child::try_wait()` or a `pidfd`), NOT via `kill(pid, 0)`. Bare PID
liveness checks are unsafe — PIDs can be reused after exit, causing the
sidecar to sample an unrelated process. Since brokkr is the direct parent,
the child handle is always available and authoritative.

### Implementation

1. brokkr creates the FIFO in scratch dir
2. Sidecar thread opens FIFO read end (guarantees reader exists)
3. Sets `PBFHOGG_MARKER_FIFO` in the pbfhogg child environment
4. Spawns pbfhogg (child handle retained)
5. Sidecar loop: sample /proc, drain FIFO markers, buffer in memory
6. Child exits (detected via child handle, not PID)
7. Sidecar bulk-inserts all data to SQLite in one transaction
8. Cleanup FIFO

### Sidecar loop (Rust pseudocode)

```rust
let mut samples: Vec<Sample> = Vec::new();
let mut markers: Vec<Marker> = Vec::new();
let mut last_hwm: u64 = 0;
let start = Instant::now();
let clk_tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as u64;

loop {
    let timestamp_us = start.elapsed().as_micros() as i64;

    // Sample /proc (3-5 file reads, ~30µs total)
    if let Some(s) = read_proc_metrics(pid, timestamp_us) {
        if s.vm_hwm_kb > last_hwm { last_hwm = s.vm_hwm_kb; }
        samples.push(s);
    }

    // Drain any pending markers from FIFO (non-blocking, independent of tick)
    while let Some(line) = try_read_line_nonblocking(&fifo) {
        if let Some((ts_str, name)) = line.split_once(' ') {
            if let Ok(ts) = ts_str.parse::<i64>() {
                markers.push(Marker { timestamp_us: ts, name: name.to_string() });
            }
        }
    }

    // Check child exit via handle (not bare PID)
    if child.try_wait().is_ok_and(|s| s.is_some()) { break; }

    // Sleep until next tick (CLOCK_MONOTONIC absolute time)
    sleep_until(next_tick);
    next_tick += interval;
}

// Child exited — bulk-insert. VmHWM from last sample, not post-exit /proc.
bulk_insert(db, uuid, run_idx, &samples, &markers, last_hwm);
```

Overhead: ~30µs per tick for /proc reads + ~50µs for FIFO drain = ~80µs
per 100ms tick = 0.08%. Negligible.

## Timeline comparison

```
brokkr results --compare-timeline <uuid1> <uuid2>
```

### Phase-aligned summary (default view)

```
Phase         | Run 1 (ee9b19f)       | Run 2 (d272b49)       | Delta
--------------+-----------------------+-----------------------+--------
STAGE1        |   82s   69MB  0 mflt  |   82s   69MB  0 mflt  |   0%
STAGE2        |  331s   69MB  0 mflt  |  301s   69MB  0 mflt  |  -9%
STAGE3        |   73s   69MB  0 mflt  |   73s   69MB  0 mflt  |   0%
STAGE4        |  461s 1573MB  0 mflt  |  461s 1573MB  0 mflt  |   0%
```

Per phase: duration, peak RssAnon, peak majflt_rate, disk read/write totals.

### Overlay view (detailed)

Align phases by marker, normalize time within each phase to [0, 1].
Overlay metric trajectories from both runs. Useful for visualizing where
a regression occurs within a phase.

## What this enables

1. **Thrashing detection**: majflt_rate spike = mmap thrashing. Dense ALTW
   Europe problem (thousands of majflt/s) instantly visible.

2. **Cache hit ratio**: rchar vs read_bytes shows how much I/O hits page
   cache vs disk. External join with BlobReader fadvise should show high
   read_bytes (fadvise forces re-read) vs without (rchar >> read_bytes).

3. **CPU utilization per phase**: which stages use all cores (rayon par_iter)
   vs one core (sequential decode).

4. **Swap timeline**: when VmSwap starts growing, correlated with markers.

5. **Allocator retention**: RssAnon growing while the process does
   constant-memory work = cross-thread alloc/free retention.

6. **Context switch diagnosis**: high nonvoluntary_ctxt_switches = CPU
   contention from other processes. Explains noisy benchmark results.

7. **Phase duration**: read directly from process-side marker timestamps,
   precise to microseconds regardless of sample interval.

8. **I/O amplification**: cancelled_write_bytes shows temp file churn.
   rchar/wchar vs read_bytes/write_bytes shows cache effectiveness.

## Future extensions

- **flamegraph integration**: trigger `perf record` during specific marked phases
- **memory map dump**: snapshot `/proc/{pid}/smaps` at markers for per-mapping detail
- **anomaly detection**: flag samples where majflt_rate > threshold or swap_kb > threshold
- **per-thread metrics**: `/proc/{pid}/task/*/stat` for diagnosing stuck rayon threads
- **live dashboard**: stream samples to a terminal UI during long runs
