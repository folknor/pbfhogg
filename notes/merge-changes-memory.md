# merge-changes: 7-OSC peak anon (research note)

Transient research note. Nothing here is binding; it may be wrong. Written
2026-10-30 under the spar procedure, research only (no edits, no new cells).

Item: `notes/pre-release.md`, "Planet RAM pass against the README table",
row `merge-changes (4914..4920, 7 OSCs)`. Planet, host bygg, commit
`627883d7`: peak anon 895 MB against the README's 2 MB, wall 30 s against
54 s (result `f24c29af`). The 1-OSC run is still 1.6 MB (`bf6db512`).

## 1. Root cause (established)

The multi-input path (`write_streaming` -> `emit_members_ordered` ->
`emit_member` in `src/commands/merge_changes/mod.rs`) makes **one whole input
the unit of both parallelism and buffering**:

- Each pool task parses an entire input, re-emits it as XML and gzips it into a
  growing in-memory `Vec<u8>` with no size hint and no byte cap.
- The vector is handed to the ordered drain only when the whole input is done,
  and written only after every predecessor has been written.
- The admission window is `rayon::current_num_threads()` (32 on bygg),
  clamped to the input count, so with 7 inputs every input is admitted at
  once. The window is a member-count bound, not a byte bound; the source
  comment says so itself ("One huge input remains unbounded").

So peak residency is roughly the whole re-emitted output still waiting to be
written. Evidence from `f24c29af`:

- Total output 962 MB; peak anon 895 MB (about 93 % of the output).
- Anon rises at about 45 MB/s from 0 to 21 s while seven vectors grow; peaks
  just before members 1 and 2 (4914, 4915) drain at 22.4 s (drops to about
  630 MB); then holds while input 3 (4916: 130 MB gz, 5.8 M changes, the
  heaviest) is still running and inputs 4-7 sit finished behind it; input 3
  finishes at 29.8 s and everything drains by 29.9 s; anon falls to 12 MB.
- Per-input completion (decompress counter timestamps): 16.8, 17.9, 19.7,
  21.7, 22.3, 25.6, 29.7 s. Avg cores over emit 5.2 of 32: seven coarse tasks
  and a shrinking tail.
- 1-OSC (`bf6db512`) streams parse -> emit -> gzip -> 8 KiB BufWriter in one
  thread, so it holds one element plus codec state: 1.56 MB.

The same structure causes the **wall** limit: wall equals the slowest single
input (about 5.6 us per change on one core; 26.3 M changes cost about 155
CPU-seconds, 29.9 s x 5.2 cores). The memory regression and the throughput
ceiling are one defect.

Codex agreed with this diagnosis on the first exchange, with one precision I
accept: peak anon is not literally the sum of compressed lengths (Vec
capacity slack, allocator retention, codec and parser state contribute), but
retained compressed members dominate. It also noted the middle input gates
ordered output by position and compute by size; moving it earlier would
release memory sooner but not shorten the tail.

### The "regression" itself

The stored cells say the 895 MB class arrived with the parallel-drain design,
not with a later change: `b6e964cc` (plantasjen, 2026-04-28, 54.8 s) and
`f4de3f8d` (plantasjen, 2026-07-14, 53.8 s) already peak at about 950 MB. The
2 MB 7-OSC cells are the fully serial era (`c612c5e6` 2.03 MB, `bef0f1fa`
2.25 MB, walls 267-273 s). See Lateral findings 1. Either way the fix is the
same: the parallel path has to stop buffering whole inputs.

## 2. Hypotheses and confidence

| # | Hypothesis | Confidence |
|---|---|---|
| H1 | Whole-input members retained until ordered drain are the peak anon | High (sidecar shape + source) |
| H2 | Whole-input task granularity is also what caps wall at the heaviest input | High |
| H3 | Inflation is a small part of the work (decompress counters sum about 7.9 s over 7 inputs, max 1.58 s) | High that it is small; the counter is reader-call time, not a pure inflate floor |
| H4 | XML reconstruction plus gzip dominates over tokenize+parse (historical 223 s serial emit of 236 s in `07ee92ee`) | Medium: different implementation and host, ratios across implementations do not isolate a stage (codex point, accepted) |
| H5 | A chunked pipeline reaches about 10 s wall here | Low-medium: a hypothesis, not an estimate. Under bounded lookahead the serial critical path can approach the sum of per-input inflate+frame times rather than the max |
| H6 | Raw splice (D below) beats chunked re-emit on wall on a 32-core host | Low: both share the same serial inflate+frame path; D mainly saves CPU |

## 3. High-conviction architectural rewrite

### R1 (priority 1). One ordered, byte-budgeted pipeline over raw element slabs ("C-prime")

**Bottleneck.** Whole-input work units: unbounded per-input output vectors,
head-of-line residency, seven coarse tasks on 32 cores, and a single-input run
that is fully serial (the `inputs.len() <= 1` branch assumes "no parallelism
available").

**Why the structure causes it.** Output order is input order, and the only
place a member boundary exists is the end of an input. Everything an input
produces is invisible to the drain until its last byte.

**Redesign.**

```
per input: inflate (MultiGzDecoder) -> XML-aware span framer
    -> raw byte slabs of complete elements, each tagged with
       (input, local seq, action context)
        | bounded, credit-admitted
worker pool: parse slab (borrowed attrs) -> validate -> normalized XML emit
    -> deflate into an independent gzip member (fused, no second copy)
        |
ordered completion window (global seq = input-major, slab-minor)
        |
output writer (prelude member, slab members, postlude member)
```

- Framer runs on dedicated input threads and does only inflate and element
  span detection (it must understand quoting, comments, CDATA and nesting;
  searching for `</node>` is not a splitter). It hands out compact byte slabs,
  not forests of owned `String`s, so semantic parsing, allocation and
  normalization move into the parallel stage.
- Each slab opens its first action section, switches as needed, and closes its
  last. Repeated adjacent `<modify>` sections are valid OSC. Gzip member
  boundaries need not match XML boundaries; the decompressed concatenation is
  what must be one valid document.
- Ordering: a coordinator assigns contiguous output sequence numbers by
  draining per-input slab queues in input order. Parser completion order never
  defines output order.
- Memory: one global byte budget. Credits are reserved before a slab is
  admitted and released only at ordered write (not when a later slab finishes
  early). The budget covers parser queues, partial slabs, in-flight workers,
  completed members awaiting drain, and a compressed-output allowance
  (compression does not guarantee shrinkage). The head input gets a reserved
  share so later inputs can never consume every credit and starve progress.
  Later inputs get bounded lookahead, not free-running parsing.
- An oversized single element is an explicit additive allowance (one element
  over budget is admitted alone).
- The single-input case uses the same pipeline; the serial branch and the
  "two gzip layouts" distinction disappear.

**Payoff.** Peak memory becomes the budget (tens of MB, chosen, independent of
input size or count) instead of output-sized. Wall drops from
"heaviest input" to max(serial inflate+frame critical path, total
parse+emit+gzip work / cores, output throughput). The 1-OSC run, today 24 s on
one core, becomes parallel too; that is likely the largest single wall win.

**Risks / correctness obligations** (codex findings, all accepted):

- Every blocking edge (queue send, credit wait, reorder wait) must wake on
  cancel and on first failure; the initiating error must not be overwritten
  by cancellation noise.
- Every admitted position must resolve (panic -> placeholder + global stop),
  or the reorder buffer waits forever.
- Inputs must be read through full gzip trailer validation; seeing
  `</osmChange>` is not proof the input is intact.
- Validation that today happens in `OwnedSink` (refusing nodes without usable
  coords, truncated input, malformed roles) must survive the move into
  workers, including checks that span slab boundaries (nesting, root close).
- Document context (namespaces, DTD entities, non-UTF-8 encoding declarations)
  affects how a slab parsed out of context is interpreted; define a supported
  profile and reject or fall back outside it.
- Output becomes many gzip members (one per slab) instead of one per input;
  output bytes depend on the slab size. Consumers must use multi-member
  readers (osmium, osmosis, gzip CLI, `MultiGzDecoder` handle it).
  `ordered_window_preserves_buffered_member_bytes` and the tunables.rs
  "output contract fact" comment change meaning.
- Liveness: framer threads blocked on full queues must not occupy threads the
  worker pool or drain needs. Dedicated input threads, a separate compute
  pool, and an independent output consumer make this tractable.

### R2 (priority 2, same pipeline). Validate-and-forward worker mode ("D")

Once R1 exists, the worker job can change from parse-and-reemit to
validate-and-forward: workers validate the slab on borrowed attributes and
forward the source bytes, then gzip. Inputs are still inflated and
recompressed (concatenating source gzip files would concatenate whole
documents, and compressed-byte surgery is not possible).

**Payoff.** Removes numeric parse/format, string allocation, entity decode and
escape per element: a large CPU and allocation saving. Wall benefit only when
workers, not the framer, are the bottleneck (contended memory bandwidth,
allocator contention, indivisible huge elements, startup/tail bubbles).

**Risks.** Needs a real OSC grammar validator, not the current driver (which
tolerates unknown content). Output preserves source spelling: coordinate
precision, delete payloads (today stripped by `write_delete_xml`), section
attributes, attribute order. Codex's position, which I accept: preserving
source bytes does not inherently make the output incorrect OSM, and nodes
without usable coords can still be refused by worker validation; it is a
preservation policy to choose deliberately, not a correctness loss.

## 4. Medium-value local changes

Each is a containment for the current structure; none fixes the wall ceiling.

### L1. Scratch-backed members

Replace each member's `Vec<u8>` with a buffered scratch file; splice in input
order after checked gzip finish. Memory drops to parser + codec + I/O
buffers. Cost: about 2x output in logical scratch traffic (962 MB written and
read back), residency shifts to reclaimable page cache. Wall roughly
unchanged, could move either way through I/O contention. Must clean up on
every exit and never publish after a failed member.

### L2. Bounded per-input output channels

Every member writes into a bounded channel of compressed chunks; the ordered
drain reads the head input's channel to completion, then the next. Caps memory
without scratch, keeps the per-input compute shape. Later workers stall
behind full queues, which lengthens the coarse tail. Merely special-casing
the head member to stream directly (with others still on unbounded vectors)
does not help this cell: the gate is input 3, and 22 s of other output is
already buffered by the time it becomes head.

### L3. Byte-aware admission

Admit by estimated output bytes (compressed input size is a decent proxy)
rather than by count. Cheap, but bounds the wrong thing: a single admitted
input is still unbounded.

## 5. Prioritized list

| Priority | Item | Kind | Fixes memory | Improves wall |
|---|---|---|---|---|
| 1 | R1 raw-slab byte-budgeted ordered pipeline, also for N = 1 | Rewrite | Yes, by construction | Yes, large (multi- and single-input) |
| 2 | R2 validate-and-forward worker mode on R1 | Rewrite variant | Yes | CPU yes; wall only if workers bottleneck |
| 3 | L1 scratch-backed members | Local | Yes | No |
| 4 | L2 bounded per-input channels | Local | Yes | Slightly worse tail |
| 5 | L3 byte-aware admission | Local | Partially | No |
| 6 | Parallel inflation of one ordinary gzip stream | Codec project | - | Defer: inflate is about 1-1.6 s per input today |

R1 makes the regression irrelevant rather than patching it; L1 is the right
choice only if a containment must land before R1.

## 6. Theoretical plan of action

1. **Cost split first-principles check (H3/H4).** From an R1 prototype, not a
   probe: time framer throughput alone (inflate + span detection, MB/s) and
   worker throughput per core (parse + emit + gzip per slab). This settles
   whether the critical path is the framer (then R2 adds little wall) or the
   workers.
2. **Framer.** Decide the supported XML profile (UTF-8, no DTD entities, no
   namespace dependence), how action context and root state are carried per
   slab, and how cross-slab validation (nesting, root close, gzip trailer) is
   proven.
3. **Budget and scheduler.** Global byte credits held until ordered write,
   head reservation, bounded lookahead for later inputs, oversized-element
   allowance. Derive slab size and budget from the host, put both in
   `src/tunables.rs`.
4. **Failure and cancel discipline.** Reuse the `FirstFailure` / placeholder
   pattern from `src/write/parallel_gzip.rs`; every wait is cancellable; all
   threads joined before return.
5. **Collapse paths.** Single-input uses the same pipeline; delete the serial
   branch and `emit_members_ordered`.
6. **Bench.** Planet 7-OSC and 1-OSC, interleaved against `627883d7`; expect
   peak anon at the budget and wall well below 30 s / 24 s.
7. **Then R2** as a worker-mode swap, benched against R1 on the same pipeline,
   with an explicit preservation policy.
8. **Afterwards, if this works:** `--simplify` (22.8 GB in `3e3ef119`) and
   `tags-filter --input-kind osc` share the owned-sink re-emit and could ride
   the same framer + worker stage; `diff --format osc` already uses
   `ParallelGzipWriter` and could converge on the same ordered output stage.

## 7. Spar record

- **Agreed first exchange:** whole-input units cause both residency and the
  compute tail; L1 works as containment; B (stream only the head) is
  insufficient.
- **Codex moved me:**
  - My owned-element batch design (C) left tokenize, allocation and semantic
    parse on the input thread; it became raw element slabs parsed in workers
    (R1).
  - The memory bound has to be end-to-end credits held until ordered drain,
    with head reservation and a compressed allowance; "slots x chunk size" was
    not a bound for element-count batches.
  - My 10 s estimate was downgraded to a hypothesis (bounded lookahead can make
    the serial path approach the sum, not the max).
  - L1 costs about 2x output in scratch traffic, not 1x.
- **I moved codex:** it first ranked D above C-prime. I argued both share the
  same serial inflate+frame path, so D saves CPU, not wall, on 32 cores,
  while needing a grammar validator and a preservation policy. Codex conceded
  and now ranks R1 first with D as a later worker mode (R2), listing the cases
  where D could still win on wall (section 3, R2).
- **Remaining disagreement:** none on mechanism. Codex holds that dropping
  normalization is a policy choice rather than a correctness loss; I accept
  that framing for R2.

## 8. Lateral findings

1. **README 7-OSC row.** No stored cell pairs "54 s" with "2 MB". The 54 s
   wall matches `b6e964cc` / `f4de3f8d` (both about 950 MB anon); the 2 MB
   cells are the 267-273 s serial era. The README row appears to splice two
   different builds. The 1-OSC row (44 s / 2 MB) matches `941a5784`.
2. **`reference/performance.md` Merge-changes section** says "at most one
   member per pool thread is alive, so memory no longer grows with the number
   of inputs". True for inputs > threads, but at planet's 7 inputs on 24-32
   threads every input is admitted and memory equals output size; the claim
   reads as a memory fix it is not at this scale.
3. **`src/write/parallel_gzip.rs` header** claims `flate2::read::GzDecoder`
   handles concatenated members. It does not (only the first member);
   `MultiGzDecoder` does. The OSC reader correctly uses `MultiGzDecoder`
   (codex finding, matches flate2 behaviour).
4. **`ParallelGzipWriter` window** at 32 workers admits up to 160 seqs x
   2 MiB = about 320 MiB raw payload. Bounded, but not small; relevant if R1
   reuses its sizing.
5. **Write pipeline permits** (`notes/cat-dedupe-memory.md`): the PBF writer
   releasing `PIPELINE_DISPATCH_PERMITS` at framing rather than ordered
   delivery is the same class of bug R1 must avoid: credits must be held
   until ordered write. merge-changes does not use that writer, so it is not
   a cause here.
6. **`-j N` builds a `JoinedPool` even for one input**, which the N <= 1 path
   never uses (scout finding, confirmed in source shape).
7. **No `*_wait_ns` counter** on the member drain, so `--stalls` reports 0
   for merge-changes; R1's reorder and credit waits should emit one.
8. **`CORRECTNESS.md`** says malformed member roles become empty in all
   sinks; `src/osc/xml_parse.rs` now errors on a malformed optional role
   (scout finding, not re-verified here).
