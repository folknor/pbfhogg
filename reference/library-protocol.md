# Library harness protocol

pbfhogg owns this version-1 protocol. The worker, `pbfhogg-harness`, links
the library of the tree under test and runs scripted sequences of API calls
in one process. The independent `pbfhogg-library` driver links devkit and
generic crates only, never pbfhogg or harness types. It owns strict reading,
process control, external judgment and the certification requirement table.

## Operator surfaces

```sh
brokkr run --with pbfhogg-harness pbfhogg-library -- list
brokkr run --with pbfhogg-harness pbfhogg-library -- verify repeat-cat --dataset denmark
brokkr run --with pbfhogg-harness pbfhogg-library -- verify --all --dataset denmark
brokkr bench --iterations 1 library-scenario --scenario repeat-cat --dataset denmark
```

The driver takes `--config PATH` (default `brokkr.toml`), `--host NAME`
(default kernel hostname) and `--harness PATH` (overrides `BROKKR_WITH_EXE`).
The harness must be an executable regular file; the driver never builds it.
Verification requires an explicit dataset; `--snapshot KEY` selects a named
snapshot, otherwise the primary (`base`). Each dataset role declares its
own variant; the driver has no variant override. Listing validates config too.

The bench measures worker process wall only, launching the worker directly
in its bench mode so sidecar sampling, markers and profiling refer to the
worker PID. Verification exists only on the run surface; the bench judges
nothing but that every call ended as its plan expects (Measurement and
profiling).

## Worker launch

- `--describe-harness`: one strict describe document, at most 1 MiB, exit 0,
  empty stderr. Discovery is bounded to 60 seconds. It performs no library
  work or writes, and never profiles.
- `--test-harness REQUEST --private-dir DIR --artefact-dir DIR`: one worker
  process, request below, private staging directory and artefact directory
  supplied as argv. `BROKKR_MARKER_FIFO` supplies the marker FIFO. The FIFO
  must be attached because call timestamps use its clock.
- `--bench-harness SCENARIO --work DIR [--dataset VARIANT PATH]...`: the
  bench's launch (Measurement and profiling below); the same FIFO rule.

The verifier clears every child's environment, so no inherited `HOTPATH_*`
reaches discovery or verification; only its own marker FIFO is supplied.
`HOTPATH_OUTPUT_PATH` remains the worker's profiling switch for the bench.
The artefact directory is never taken from an environment variable.

## Documents

Every document either side writes is one JSON object in UTF-8 without a
byte order mark, and nothing follows it but whitespace. Its schema is
closed:

- an unknown key, a duplicate key, a missing required key, a key the
  object's shape forbids, and a value of the wrong type are each refused;
- integers are JSON integers, without fraction or exponent (and never
  `-0`), in the range a field states;
- an object is never accepted in array form.

Each refusal names the path into the document, such as
`describe.scenarios[0].roles[2].owner`. The exceptions are text that is not
JSON at all and a number beyond the range of a double (`1e400`), which the
JSON parser itself refuses before any path exists; those name a line and
column instead.

Every document the harness writes carries `protocol`, an integer. The
independent driver implements 1 and refuses any other value, naming both.

A hotpath report is not a protocol document: the hotpath crate writes its
own JSON without `protocol`. Profiling belongs only to the measurement bench.

### Identifiers

Scenario, call, role and assertion ids match `[a-z0-9][a-z0-9_.-]{0,63}`:
each is usable as a FIFO call id (`brokkr man measure`, Marker and counter
rules), a file name and a command-line word without escaping. `setup` is
never a call id: it names the harness's own preparation. Assertion ids
starting `brokkr.` are reserved for results the driver adds itself.

A capability id is an identifier ending `.v<N>`, N a positive decimal
without a leading zero (`cancel.pre-decision.v1`); a capability whose
meaning changes gets a new id.

One-line text (a description or a label) is 1 to 200 bytes with no control
character.

## The describe document

```json
{
  "protocol": 1,
  "scenarios": [
    {
      "id": "repeat-cat",
      "description": "cat the input twice in a row",
      "deadline_s": 600,
      "roles": [
        {"id": "input", "kind": "pbf", "source": "dataset", "variant": "indexed"},
        {"id": "out-a", "kind": "pbf", "source": "output", "owner": "a"},
        {"id": "out-b", "kind": "pbf", "source": "output", "owner": "b"}
      ],
      "calls": [
        {"id": "a", "label": "cat input -> out-a", "stream": ["counters.v1"]},
        {"id": "b", "label": "cat input -> out-b"}
      ],
      "assertions": [
        {"id": "a.outcome", "call": "a"},
        {"id": "b.outcome", "call": "b"},
        {"id": "outputs-equal", "call": null, "covers": ["repeat.equal-outputs.v1"]}
      ]
    }
  ]
}
```

- `scenarios`: at least one, ids unique.
- `description`: one line.
- `deadline_s`: 1 to 86400, the wall one process of the scenario gets.
- `hotpath` (optional, default false): the scenario can be profiled (see
  Measurement and profiling). True requires exactly one call.
- `calls`: at least one, ids unique within the scenario, none `setup`. Each
  has a one-line `label`, and may have `stream`: a list of capability ids,
  each at most once, that the call's stream verdict stands for. A nonempty
  list makes the driver add `<call>/brokkr.stream` (see The call stream), which
  covers those capabilities in a certification; absent and `[]` mean the
  same. There is no CLI replay.
- `roles`: every path the scenario uses (the discard sink excepted, see
  Discard sink), ids unique within the scenario; possibly none.
- `assertions`: the scenario's assertion inventory, ids unique within the
  scenario; possibly none. `call` is required: a call id of this scenario,
  or `null` for an assertion about the scenario as a whole. `covers` is
  optional: the capability ids, each at most once, that the assertion's
  result stands for in a certification (see Certification); absent and `[]`
  mean the same.

`cli`, `counterpart` and `isolation` fields are refused, even when null
or false.

### Roles

| `source` | `kind` | `owner` | `variant` | `seeded` |
|---|---|---|---|---|
| `dataset` | `pbf` | forbidden | required | forbidden |
| `generated` | `pbf`, `file`, `directory` | forbidden | forbidden | forbidden |
| `output` | `pbf`, `file`, `directory` | required | forbidden | optional |
| `scratch` | `directory` | required | forbidden | forbidden |

- A `dataset` role is a dataset file of the run in the named variant: read
  only, and shareable between calls. `variant` is a variant name
  (`[A-Za-z0-9_.-]`, 1 to 64 bytes). Dataset roles are PBFs only.
- A `generated` role is written by the harness's setup before any call and
  read by calls; setup owns it.
- An `output` role is written by its owner; a `scratch` role is its owner's
  run scratch. `owner` is a call id of this scenario, exactly one, so no two
  calls write one path.
- A `seeded` output role (`seeded: true`; default false) starts as its
  **seed**, which the driver writes before the launch, so its call meets an
  existing destination: replacing one, or refusing to. The seed of role R
  is `brokkr-library-seed-v1 R` and a newline: a regular file holding
  those bytes for a `file` or `pbf` role, a directory holding only the
  regular file `seed` with those bytes for a `directory` role. It is no PBF;
  the role id in it makes one member's seed restored in another's place
  visible. A seed is reserved content: no call publishes a destination
  byte-identical to its role's seed (producer obligation), so a destination
  that holds it holds no published generation: it was never replaced, or a
  rollback put the seed back.
- `kind: pbf` declares a regular PBF file. The independent driver does not
  decode it or compare it with a CLI output.

## Running a scenario

The driver allocates `SCRATCH/tools/RUNID/`, never the old clean-owned
library scratch directory. Permanent create-new reservations in
`SCRATCH/tools/.library-run-ids/` prevent reuse even after a passing run is
removed. The layout is:

- `request.json`: immutable request bytes;
- `cwd/`: initially empty, also the private staging directory, empty on return;
- `artefacts/`: initially empty;
- `roles/`: bound output, generated and scratch role names.

Dataset bindings are absolute resolved paths. Output roles start absent,
except seeded outputs. Scratch and generated directory roles start empty;
generated file and PBF roles start absent. A sibling FIFO is owned and
removed by the driver. Private staging is separate from output parents.

```json
{"protocol": 1, "scenario": "repeat-cat", "iteration": 0,
 "bindings": {"input": "/abs/data/denmark.osm.pbf", "out-a": "/abs/run/roles/out-a", "out-b": "/abs/run/roles/out-b"}}
```

`bindings` names every role and nothing else. `iteration` is an integer
0 to 2^63-1, validated but deliberately unused by the worker. Both current
surfaces send 0. It cannot affect a workload, plan or context. Private and
artefact directory paths are not request fields.

### Dataset resolution

Acquisition and registry writes are documented in [datasets](datasets.md);
completed producer outputs use [snapshot promotion](dataset-promotion.md).

`pbfhogg-devkit` composes recursive includes, the optional user-wide layer,
`[host.H.datasets.D]` and legacy `[H.datasets.D]`. A host present in both
namespaces is refused. Includes resolve relative to their canonical source
file (a symlink target included); dataset paths resolve through host `data`
(default `data`) relative to the root config's project directory. Scratch
uses the same host namespace and relative-path rules. Named snapshots never
fall back to primary PBF entries. The user layer permits only personal
convention keys, never datasets; unrelated tool sections are merged without
being validated or executed. OSC, PMTiles and polygon resolution are outside
this PBF-role resolver.

The executable boundary passes explicit host and environment values to
devkit. `BROKKR_USER_CONFIG`, otherwise `XDG_CONFIG_HOME` or `HOME`, selects
the user layer. Configured `xxhash` pins are checked using bounded streaming
XXH3-128; uppercase pins normalize to lowercase. Retired `sha256` pins are
refused. There is no digest cache; ordinary concurrent changes/replacements
are checked. Every input identity keeps expected (optional) and observed
(always computed) digests separately, even when unpinned.

### Quiescence and deadline

The driver is the sole spawner and sets `PR_SET_CHILD_SUBREAPER` before any
launch. It enforces each scenario's `deadline_s`, drains bounded stdout,
stderr and FIFO captures throughout execution and cleanup, then enumerates
all tasks' children through `/proc/self/task/*/children`, opens pidfds before
signalling, SIGKILLs and reaps with `waitpid(-1, __WALL | WNOHANG)`. Numeric
PIDs are never used for signalling. Adopted children, zombies included,
fail the run even if cleaned. Only `ECHILD` establishes quiescence; a zero
wait result does not. Enumeration, signalling, reaping or cleanup-bound
failure means quiescence unestablished: keep the run and bounded evidence,
read no artefacts, issue no verdict and stop every subsequent sweep launch.
Bounds live in the driver's independent `tunables.rs`.

## The run protocol

Exit 0 means every declared result ran and passed. Exit 1 means at least
one result failed or was unevaluated. Exit 2 refuses the request and may
write `refusal.json`, `{"protocol": 1, "reason": ONE_LINE}` (1 to 1000
bytes). Other exits, signals, deadlines, any stdout/stderr or capture loss
fail verification. Diagnostics belong in documents, never output streams.

Producer obligations: read only the request, input roles and private
staging; write only owned roles, artefacts, private staging, a requested
profiling report and the discard sink (see Discard sink). Setup is
deterministic and precedes calls. The schedule
honours ownership. Output changes use publications; publications sharing a
role run in ordinal order, rollback included. No call publishes its seed.
Apply declared process policy before library work and record effective
settings in contexts. Never reset SIGPIPE to default. Drop call values and
wait for attributable callbacks before closing their document. Filesystem
checks detect surviving state, not reads or writes undone before exit.

## The artefacts

Read on exit 0 and 1 only. `artefacts/` holds exactly `scenario.json` and
`calls/`, and `calls/` exactly one `<call id>.json` per declared call; any
other entry, a symlink, a special file, or a document over 16 MiB fails the
run. No file is followed through a symlink. Every document follows
Documents above.

**`scenario.json`**: `protocol`, `scenario` (the scenario's id), `plan`,
`assertions` (the scenario-level results) and `diagnostics`, setup's and the
harness's own events (never a call's), in the call documents' shape. Its
collection closes after setup and every call document; its `at_us` readings
are held to their range only, since no call reading anchors them.

**`calls/<id>.json`**:

- `protocol`; `call`, the file's call id.
- `entry_us`, `return_us`, `seal_us`: `MarkerFifo::now_us()` at command
  entry, at return, and when sealing ended; each 0 to 2^63-1, with
  `entry_us <= return_us <= seal_us`. `return_us - entry_us` and
  `seal_us - entry_us` must each be at most the process wall the driver observed
  (spawn to the ending it saw) plus one second. The bound is relative on
  purpose: readings of one opened FIFO share its clock, whose origin is not
  reliable. That the readings sit at those boundaries is the harness's
  obligation.
- `outcome`: `{"status": "ok"}`, `{"status": "cancelled"}`, or
  `{"status": "error", "kind": ID, "message": ONE_LINE}` (an identifier,
  and 0 to 1000 bytes). Reported; the harness's assertions judge it.
- `publications`: every publication the call decided, in order, each
  `{"ordinal", "decision", "members"}`: ordinals 1, 2, ... without gaps;
  `decision` `authorized`, `refused_cancelled` or `refused_error`;
  `members` the destinations it covers, at least one, each
  `{"role", "effect"}`: `role` an output role this call owns, at most once
  per publication (an output set is one publication of several members, an
  output directory one member of a `directory` role); `effect` what that
  attempt left at the destination when it finished, relative to the
  destination just before it:
  - `none`: as it was, a rollback to absence included;
  - `installed`: the new generation is there, an incomplete rollback
    included;
  - `restored`: the previous generation was moved aside and put back;
  - `displaced`: the previous generation was moved aside and not put back,
    so the destination is absent. The moved-aside generation lives in the
    private staging directory and must be cleaned before return.

  Every member of a refused publication is `none`. See Publications on
  disk for what the driver holds them against.
- `cancel`: `null`, or what `Invocation::cancel()` returned:
  `{"result": "before_decision", "ordinal": K}` (K a `refused_cancelled`
  publication), `{"result": "after_decision", "ordinal": K}` (K an
  `authorized` one), or `{"result": "no_publication_pending"}`. After a
  cancel's K, every publication is refused. The driver checks this consistency
  and nothing else of order; order judgments are the harness's assertions.
- `seal`: `{"state": "sealed"}`, `{"state": "outstanding", "held":
  {KIND: N, ...}}`, or `{"state": "refused", "reason": ONE_LINE}` (1 to 1000
  bytes). `held` is the census at the seal attempt: every value the caller
  still held (a session, a buffer, a bound output, an escaped writer, ...),
  each counted once under one producer-defined kind, an identifier, N 1 to
  2^64-1; at least one kind. The harness keeps the census it took then,
  although it drops those values before writing the document.
- `snapshot`: when sealed, the call's final counters, name to an integer
  0 to 2^64-1, each name 1 to 400 bytes without `=` or a newline (so its
  sealing record fits a FIFO line); otherwise `null`.
- `stream`: `{"state": "sealed"|"failed"|"open", "drops": N}` (N 0 to
  2^64-1), the call sink's own account: how it ended, and every line it
  dropped, its terminal record included. It is `open` exactly when `seal` is
  not `sealed` (a sink is ended only by a successful seal of its call's
  invocation); anything else fails the run. The call stream says how the driver
  holds it against the FIFO.
- `context`: a nonempty object, every effective per-call setting.
- `diagnostics`: the call's captured `host::Diagnostics` events, in capture
  order, each `{"at_us", "kind", "key", "message", "truncated"}`. `at_us`
  is `MarkerFifo::now_us()` when it was captured, 0 to 2^63-1, never before
  `entry_us` (an earlier event is setup's, which goes in `scenario.json`),
  and `at_us - entry_us` within the process wall the driver observed plus one
  second, as for the other readings. `kind` is an identifier, the event
  kind's name in lower case (any kind, so a new one needs no driver
  change); `key` is 1 to 200 bytes; `message` is at most 4000 bytes, and a
  longer one is cut by the harness at a character boundary with `truncated`
  true. Equal readings establish no order; diagnostics do not decide the
  verdict. **The cut-off is the document**: a call's values can still report
  after its seal (a writer dropping, an unused output cleaning up), so the
  harness drops every value of the call, waits for every callback
  attributable to it to return, then closes and snapshots the call's
  collection, and only then writes the call document. An attributable event
  that misses that is a producer defect; it never moves to `scenario.json`.
- `assertions`: this call's results.

**Results**: `{"id", "status", "detail"}`, `status` `pass`, `fail` or
`unevaluated`, `detail` any text of at most 4000 bytes. Across the
documents the results are exactly the inventory: one per declared
assertion, in `scenario.json` for `call: null`, else in its call's
document. A missing, duplicate, undeclared or misplaced result fails the
run, naming it.

**Publications on disk.** Once the run directory passed its layout check,
the driver replays each output role of each call from its initial state
(absent, or its seed) through the members naming it, in ordinal order:
`installed` leaves a published generation, `restored` and `displaced`
need a destination to move aside (one reported on an absent destination
fails), `displaced` leaves it absent, `none` leaves it alone. The state that
leaves must be what `roles/<role>` holds: absent; byte-identical to its
seed; or a published generation, present, of the role's kind and not its
seed. Any other fails the run, naming the call and the role. This proves
the destination is in the state the effects describe, never which
generation a published one holds: that a restore after an earlier
installation put back the right generation, and that a displaced
generation's backup survived, are the harness's assertions to make.

**Plan and context** are the harness's to define; the driver interprets
neither. Each stays in a plain value domain: objects with unique keys,
arrays, strings without NUL, booleans, `null` and integers in -2^63 to
2^64-1 (no other number, and not `-0`). `plan` holds `calls`, an object
whose keys are exactly the declared call ids, each a nonempty object. No
string in either, object keys included, may contain the run directory's
absolute spelling or a bound dataset file's (as resolved, or canonical).
That is a narrow check on the value domain, not proof that the plan is free
of run-specific paths: relative spellings and split strings pass it.

Producer obligations, which the driver does not check: the plan carries every
choice that affects the workload (each call's command and options, its
inputs by role, the Host each call gets, the order, grouping and
concurrency of the calls, setup and every generated input's recipe), and
each call's context every effective per-call setting (Resources figures,
executor, page-cache policy, process actions, the soft `RLIMIT_NOFILE` at
its start). Neither carries anything run-specific: no path, timestamp,
iteration or pid. The verification identity records them. Calls the plan
runs concurrently come with a harness assertion that their lifetimes intersected; nothing claims they ran in
parallel.

## The call stream

Each call writes its markers and counters to the FIFO through a sink of its
own, tagged with its call id (`<ts> %<call> <name>`, `<ts> %<call>
@<name>=<value>`; `brokkr man measure`, Marker and counter rules). When the
call's invocation seals, its sink ends the stream with two more record
kinds, which only a tagged record can carry:

- a **sealing emission** `<ts> %<call> @@<name>=<value>` for every counter
  of the call's snapshot, zeros included, its value the snapshot's
  saturated to 2^63-1 as the wire carries it;
- then one **terminal record**, `<ts> %<call> !sealed drops=N` or
  `<ts> %<call> !failed drops=N`, N the lines the sink dropped before it.

Producer obligations: a sink is ended only by a successful seal of its
invocation, and an invocation that does not seal leaves its sink `open`
and writes neither kind; sealing begins after the sink stopped admitting
ordinary records and every callback into it returned; one thread stamps and
writes the sealing emissions and then the terminal record, in that order;
nothing of the call follows the terminal record.

A tagged record naming a call id the describe document does not declare
fails the run, whatever its exit: lines arrive whole or not at all, so no
loss can make one.

The driver judges each call's records, in the order they arrived, against its
call document:

- **Proven defects fail**, whatever was lost: a record stamped outside
  `entry_us..seal_us`; a sealing or terminal record stamped before
  `return_us`, or before the sealing record ahead of it; an ordinary record
  stamped later than any sealing or terminal record, or arriving after
  sealing began; a sealing emission naming a counter an earlier sealing
  emission of the call already named; a second terminal record, or any
  record after it; a terminal record whose state or drops differ from the
  artefact's `stream`; no terminal record from a sink the artefact reports
  `sealed` or `failed`, although the reader lost nothing and the artefact
  counts no drop; a sealing emission naming a counter the
  snapshot lacks, or carrying another value; an ordinary counter of a
  snapshot name above its final value; any sealing or terminal record of a
  call that did not seal.

  Ordinary records may repeat a name freely: a marker emitted again (a
  `WAIT` pair per wait), a counter sampled many times or emitted once per
  stage is legal, held only to the snapshot bound above.
- **Otherwise incomplete is unevaluated**: a call that did not seal, a
  capture that may have lost a FIFO record (a rejected line, a read error,
  records left unread, the marker or counter limit), no terminal record, or
  one that reads `failed` or counts drops.
- **Otherwise complete**: it passes when every snapshot counter has its
  sealing emission, and fails naming the ones that have none.

For a call whose `stream` lists capabilities, the driver adds the verdict as
`<call>/brokkr.stream`, with reasons as its detail. A non-pass fails verification.

## The verdict and retained evidence

Only after quiescence does the driver admit exactly the run layout above:
request unchanged, original `cwd/` empty, only declared role names, each
absent or of its declared kind. Symlinks and special files are refused;
replaced directories are not traversed. Artefacts are read only for exits
0 and 1. Strict documents, assertion inventory, clock bounds, stream/seal
accounting and publication replay must all pass. Exit 0 with a non-pass
harness result, or exit 1 with none, is inconsistent. `unevaluated` fails
as `fail` does, while remaining distinct in records.

The driver adds `<call>/brokkr.seal` (pass only when sealed) and
`<call>/brokkr.stream` for declared stream coverage. Driver results do not
change whether the worker's own exit is consistent. The reserved `brokkr.*`
assertion namespace, wire name `brokkr-json-v1` and seed recipe remain
unchanged: an ownership move is not a wire-format revision.

Passing verification is quiet and removes its run directory. Failures keep
`evidence/` with bounded stdout, stderr, raw FIFO, capture flags, reasons and
identities. Evidence writes pin the original run and new evidence directories
by descriptor, refuse existing entries and follow no planted link.
Unestablished cleanup remains that class even if evidence writing fails;
no artefact-derived result or plan/context identity is issued then.
There is no startup cleanup or garbage collection by the driver.

## Certification

The driver owns the 17 requirements in `crates/library/src/requirements.rs`,
independent of worker declarations and config. Its declaration test reads
`crates/library/fixtures/describe.json` as a supplied serialized document,
never through harness types. Refresh that fixture with
`brokkr run pbfhogg-harness -- --describe-harness` when declarations change.

The sweep runs in describe order and resolves each scenario's dataset roles
before its launch. Resolution failures are recorded as scenario failures;
ordinary failures do not stop later scenarios. Unestablished cleanup stops
further launches. Unlaunched declarations remain present with missing
results. Coverage comes only from assertion `covers` and call `stream`:

- failed: any covering result is fail, unevaluated or missing;
- otherwise passed: at least one covering result passed;
- otherwise unsupported: no declared covering result.

Unsupported fails certification. A passed covering result counts even if
its scenario failed for another reason; every scenario must still pass.
Extra capability ids are recorded but not required. Certification requires
every scenario and every required capability to pass.

The requirement digest is `require-v1:` plus XXH3-128 of sorted ids, each
followed by newline, meanings excluded. Changed meaning requires a new id.
A record retains the applied ids and meanings, digest, classifications and
covering results, scenario failures and identities, expected/observed input
digests, dataset/snapshot/host, completion state and reserved run id. It hashes
the worker executable before discovery and again after the sweep; a changed
executable cannot certify. It claims no unverifiable build commit, feature
or profile provenance.

Stdout JSON and `SCRATCH/tools/library-certifications/RUNID.json` carry record
kind `pbfhogg-library-certification`, version 1. Persistence is create-new
through a pinned no-follow directory, with file and directory sync; existing
records are never overwritten or removed. Certification ids share the
permanent run reservation namespace. Persistence failure fails the command.

## Measurement and profiling

`library-scenario` benchmarks and does nothing else. It makes one measured
call, `pbfhogg-harness --bench-harness SCENARIO --work DIR` followed by
`--dataset VARIANT PATH` for every bound `--inputs` variant (default `all`
of the selected snapshot's PBF table; narrow it with `--inputs indexed`
when the scenario needs only that). It reads no describe document, knows
nothing of roles or seeds, sets no deadline and records nothing.

In bench mode the worker prepares its own input: it binds each dataset role
to the `--dataset` file of its declared variant (refusing a scenario whose
variant was not handed over), leaves generated and output roles absent
under `DIR/roles`, writes a seeded output's seed, and uses `DIR/private` as
its private directory. It then runs the workload exactly as `--test-harness`
does, observation hooks included since they shape what runs, and stops:
no call context, publication mapping, scenario judgment, inventory
completion or artefacts. The one check it keeps is the plan's: once the
whole schedule has run, every call must have ended as its plan's `expect`
says (see Planned endings). A planned error is part of the workload; an
unplanned ending (a corrupt input that fails a call planned ok, which would
otherwise time as a fast success) is exit 1, with empty stdout and stderr.
Exit 0 means the schedule ran and every call ended as planned; a refusal is
exit 2 with empty stderr.

The measurement is the whole worker process wall, setup and generated-role
construction included. Every scenario's call is marked `hotpath = "worker"`,
so `--hotpath` and `--alloc` profile the whole worker, every call of a
multi-call scenario together; the describe document's `hotpath` flag (one
call, no generated role) says only which profiles attribute to a single
call. Workload choices are deterministic; varying inputs are bound params
defining arms.

## Planned endings

This section is the harness's own convention, not something the driver
reads. Every call's plan entry carries `expect`: `{"status": "ok"}`,
`{"status": "cancelled"}`, or `{"status": "error", "kind": ID}`, in the
classes of the call document's `outcome` (a cancelled error is
`cancelled`, never an error of kind `cancelled`; `other` is never planned).
Every scenario carries the scenario-level assertion
`calls-ended-as-planned`, which fails naming each call whose outcome is not
its `expect`. A verification therefore holds the same rule a bench does,
plus everything else it judges, so a scenario cannot certify while its
measurement fails.

## Discard sink

A call whose command writes into a discard sink (the `encode-*`
scenarios; plan `"sink": "discard"`, set by the typed command, not free
text) opens `/dev/null` for writing. It is a character device, not a role:
the exemption covers both the rule that `roles` names every path the
scenario uses and the producer obligation to write only owned roles,
artefacts, private staging and a requested profiling report. No
publication is made for it. Like the rest of the plan, this is a producer
obligation the driver does not check.
