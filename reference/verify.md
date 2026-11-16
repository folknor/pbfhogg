# pbfhogg-verify: cross-validation contract

`pbfhogg-verify` (`crates/verify`) cross-validates the pbfhogg CLI against
osmium, the one reference tool. It replaces pbfhogg's part of
`brokkr verify`, with the complete suite available here.

```sh
brokkr run --with pbfhogg pbfhogg-verify -- sort --dataset denmark
brokkr run --with pbfhogg --with-commit REF pbfhogg-verify -- all --dataset germany
```

`--with-features LIST` builds the CLI under test with features (for
`--direct-io`). The verifier never calls brokkr and links no pbfhogg, so no
verdict rests on pbfhogg code compiled into the verifier; verifier-owned
readers and models over the inputs and reference-tool output are the
independent evidence. Nor does a verdict rest
on the pbfhogg CLI judging its own output, except at the residues listed in
"Where pbfhogg is still a comparator" below.

## How outputs are judged

- **Element equality** is the verifier's merge of osmium's decodings
  (`opl_compare`): `osmium cat -f opl[,add_metadata=false][,locations_on_ways=true]`
  of each side to a file in the check's directory, then a streaming merge
  keyed by kind and ID in osmium's object order (kind node, way, relation;
  then ID zero, negative IDs by magnitude, positive IDs by magnitude) that
  requires each side strictly ascending, so it also certifies, as osmium
  decodes them, that neither side repeats or misorders an object, and
  compares every record line byte for byte. Repack and degrade use the same
  merge walk without the order requirement (`compare_sequence`, as osmium
  diff walks its inputs): no unmatched or differing record then means the
  two record sequences are equal element by element, in any order; that is
  their claim, not identity under a sort. Differing identities are
  classified first-only, second-only or differing and written to a listing
  beside the outputs, both sides' records included; the OPL pair is removed
  once merged and kept only when the merge itself fails. This is equality of osmium-decoded records under a field
  policy, not of wire fields: absent metadata becomes libosmium's defaults on
  both sides, timestamps are whole seconds, coordinates are at 1e-7 degrees
  and a string is cut at an embedded NUL. OPL escapes its separators, so tag
  boundaries are compared (osmium diff's checksum feeds tags without
  lengths, so `a=bc` and `ab=c` collide there; no check judges with osmium
  diff). Metadata is compared everywhere except the `diff --format osc`
  roundtrips; inline way coordinates are compared when both files declare
  `LocationsOnWays`, except in apply-changes locations mode. Where
  coordinates are the command's promise (add-locations-to-ways), the check
  requires both declarations. Where pbfhogg keeps the input's declaration
  (cat, `getid --invert`, sort), the osmium reference is written with
  `-f pbf,locations_on_ways=true` when the input declares it, and every
  osmium output or sorted copy written with that option must declare it (a
  verifier error otherwise), so a kept declaration is compared coordinate
  by coordinate.
- **The field policies are held to real data** by `field-policies`, the
  first check of the suite, which runs no pbfhogg. On a slice of the input
  built by osmium (the first IDs of each kind's first data blob, `getid -r`
  for nodes and ways, plain `getid` for relations, `add-locations-to-ways`
  over both), osmium's OPL decoding with metadata and coordinates is edited
  one record and one field class at a time: version, timestamp, changeset,
  uid, user, a tag value, a tag boundary, a node coordinate, a way ref, an
  inline way-node coordinate, a member type, ref and role. Each edit is
  written back by osmium and compared with the unedited copy under FULL,
  NO_LOCATIONS, CONTENT and `Policy::between`: a policy that claims the
  class must report exactly that identity as differing, one that does not
  must report identical. Which field changed is read back from osmium's own
  decoding over the whole record, not from the comparator under test, and
  every other record must be unchanged. `between` is held to all four
  declaration combinations and to a pair differing only in inline
  coordinates. Visibility is not controlled: libosmium writes it only to
  history files. A class with no candidate record is named in the PASS
  line; any control that fails or cannot be built is a verifier error
  (`ERROR`, exit 2), which stops a suite, since every later element verdict
  rests on the comparator. Which policy each check passes is not
  established by it; the generated protocols in `crates/verify/tests`
  establish it for some checks, their fake osmium decoding by the
  requested OPL options: a version-only change of pbfhogg's output fails
  extract, multi-extract, cat, tags-filter, repack, degrade and
  apply-changes (both modes) and passes the derive-changes roundtrip; an
  inline-coordinate-only change fails repack and degrade and passes
  apply-changes in locations mode. The other checks' choices are not yet
  witnessed.
- **The `LocationsOnWays` declaration** of each output of sort, cat,
  tags-filter, getid (plain, `--invert` and `--add-referenced`),
  getparents, extract, multi-extract, renumber and apply-changes is judged
  against the command's row in the output-header table of
  `docs/cli/commands.md`, on the file pbfhogg wrote, so that a lost
  declaration cannot switch the element comparison to one without
  coordinates and pass. An output never declares it when the input does
  not. On an input that declares it: `cat --type` on an input whose first
  data blob carries indexdata, `getid --invert` without a way ID, and
  `apply-changes --locations-on-ways`, must keep it; `getid --invert` with
  a way ID must drop it with the loss warning. sort is decided from the
  input's per-blob indexdata claims: two overlapping non-negative way ranges
  cannot both be copied whole into order, so it must drop it with the
  warning; with no overlap, every data blob indexed, no negative or empty way
  claim and the sort claim, it must keep it; otherwise (no sort claim, where
  an internally unsorted blob is rewritten and only a decode shows one, or
  claims the overlap argument does not cover) it may keep it or drop it with
  the warning, and that drop is not shown to be required. The rest must drop it with the
  loss warning. The warning is read from pbfhogg's stderr as the CLI renders
  `header.way_metadata_loss` (`Warning: the input declares ..., which the
  output does not declare`, `LocationsOnWays` a member of the list).
- **Order and counts** of an output come from its osmium OPL decoding
  (`opl_compare::order`, no metadata or coordinates): a file is in strict
  order when its objects ascend by kind and then ID with none repeated.
  `osmium fileinfo -e` cannot serve here, since it refuses a file whose ways
  lack their nodes, as every way-only or filtered output does; it still
  reads whole inputs for check-ids, repack and degrade.
- **Header claims** (`Sort.Type_then_ID` and `LocationsOnWays` among the
  optional features, and the sort feature in either list) are read by the
  verifier's own `OSMHeader` reader (`pbf_frames::header`).
- **Sorted copies** are written by `osmium sort`, with
  `-f pbf,locations_on_ways=true` when the source declares
  `LocationsOnWays`, so inline coordinates survive into the comparison; a
  copy that then does not declare it is a verifier error.
  Copies fed to pbfhogg use osmium's default `simple` strategy, the only one
  that writes the `Sort.Type_then_ID` claim pbfhogg branches on. The `sort`
  check's own reference uses `-s multipass`: it judges elements and
  pbfhogg's order, never the reference's header, and `simple` holds the whole
  input in memory (osmium documents roughly ten times the PBF size), which
  the kernel OOM killer ended on germany. Multipass holds one object type at
  a time; its observed peak on germany is in the evidence below. A sorted
  copy of an unsorted germany-sized input still takes the simple strategy
  and its memory.

## Inputs

- `--dataset` (default `denmark`), `--variant` (default `indexed`),
  `--snapshot KEY` resolve through `pbfhogg-devkit` from the composed
  `brokkr.toml`, pins checked.
- Real-data verification runs on denmark and germany only. Every
  dataset-backed input is refused for any other dataset; planet and the
  continents are bench-only. The policy is keyed on the dataset name, never
  inferred from file size. Germany is the scale upper bound: on it the
  osmium sort reference was OOM-killed, OPL decodings filled scratch and
  renumber met far more orphans than on denmark. A new region is a
  one-line change to the list in `crates/verify/src/main.rs`.
- `--input PATH` names a fixture PBF instead (relative to the project root).
  It exempts only that PBF, never a dataset-resolved OSC or base.

The `all` command also accepts `--osc-seq SEQ`, `--osc PATH` and
`--bbox=WEST,SOUTH,EAST,NORTH`. `--osc` and `--osc-seq` conflict. An explicit
fixture OSC exempts only itself from dataset eligibility. Bboxes require
four finite coordinates and preserve their original spelling. Only `all`,
`extract` and `multi-extract` accept `--bbox`; change-only selection never
implies geographic filtering.

The suite reports every entry in order, including explicit `SKIPPED`
lines counted separately from passes. Extract skips without either a bbox
or polygons; multi-extract skips without a bbox. The six OSC-consuming
entries skip only when no OSC is configured. The two locations entries,
`degrade -l` and `apply-changes -l`, also skip for a fixture PBF or an
absent selected-generation `altw-keep-untagged` base; `degrade -l` needs
no OSC. Configured invalid inputs fail resolution, even
when another missing prerequisite would have skipped the check. Snapshot
fallback never substitutes the primary keep-untagged base.

Selection distinguishes absent configuration from invalid configuration. A named snapshot with no OSC entries uses the
primary OSC table, narrated on stderr; a configured invalid entry or a
missing requested sequence in a nonempty snapshot table fails instead.
Omitting a sequence requires exactly one configured OSC; an empty table is
absent. Inclusive ranges require every sequence. Dataset polygons and the
selected generation's `altw-keep-untagged` PBF have their file digests and
configured pins checked. A fixture PBF skips the suite's keep-untagged base.

## The executable under test

`--pbfhogg PATH`, else `BROKKR_WITH_EXE`. The run copies it into its own
directory, hashes the copy (printed on stderr) and runs only the copy, so a
rebuild during the run cannot mix executables. The verifier targets the
current CLI contract and does not probe flags: an executable that refuses an
argument fails the check that passed it.

Each pbfhogg launch writing an output gets a private `--staging-dir`; a
launch whose invocation shape writes run scratch gets a private `--tmp-dir`.
`--overwrite` is never passed: every output lands in a fresh directory.

## Run directory

`SCRATCH/tools/verify/RUNID/` (SCRATCH is the host's `scratch`, default
`data/scratch`). RUNID is time, process ID and random bits, created
exclusively, never reused. Each check writes under `checks/NAME/`; each launch
gets its own private directory there. A run in which every executed check passed is
removed unless `--keep`; any other run is kept and its path printed.

## Processes

The verifier is single-threaded and owns every child it starts. It is a
child subreaper; stop signals (SIGTERM, SIGINT, SIGHUP) arrive on a signalfd
polled beside each child. Each launch settles before its output is read or
another launch starts: the leader's status is collected, then any surviving
descendant is killed and reaped until none remains.

- A descendant that outlived its leader makes the check ENVIRONMENT: its
  outputs may be partial.
- A tree that cannot be reaped stops the run (exit 2, directory kept).
- A stop signal kills the running tree at once and exits 130. Every child
  carries a parent-death SIGKILL; a descendant forked by a launcher does not
  inherit it and may survive a verifier killed by SIGKILL. Nothing is
  promised after SIGKILL.

Stderr capture is bounded to 1 MiB. Exceeding the bound is a verifier error;
the launch is killed and settled immediately, even if it keeps writing.

Children start from an empty environment. Reference tools get a fixed `PATH`
(`/usr/bin:/bin`), a C UTF-8 locale, UTC and a launch-private `HOME` and
`TMPDIR`; osmium is found at startup on the caller's `PATH` (absolute
entries only).

## Input identity

`brokkr run` holds the machine lock for the verifier's whole life, which
keeps every cooperating writer (the dataset tool, benches, another
verifier) away. Each input's identity (device, inode, size, mtime, ctime) is
recorded when resolved and compared after every reference run and every
check; a change makes the check ENVIRONMENT. That detects ordinary external
replacement; concurrent namespace manipulation by a process outside the lock
is unsupported.

## Reference cache

Completed reference executions are stored at
`SCRATCH/tools/verify-reference/` (format 2). No pbfhogg output, observation,
or verdict is cached: every comparison runs again. The element comparison's
OPL decodings are not cached either: each is as large as its input, read
once and removed, and publishing them filled a germany run's scratch. A hit is shown as
`osmium sort cached` in the check's steps; `--verbose` also shows the hit in
detail.

The key includes the declared run ID, literal arguments, input content and
filename suffixes, output artifact names, controlled environment, executable
bytes and version, and the shared-library closure resolved by `ldd`.
Identity probes use the same lifecycle and private environment as tool runs.
When identity cannot be established, the reference runs uncached. Utilities
started by launcher scripts and libraries loaded outside the `ldd` closure
are outside that identity.

Only accepted exits with every declared output present can be published.
Inputs and tool identity are checked again after execution; input movement
is ENVIRONMENT. After all children settle, cancellation and input identity
are checked immediately before the completed staging tree is renamed into
place. Staging paths include RUNID; sweeps exclude the current run's staging
and removals never follow symlinks. Published artifacts and manifests are
immutable. Lookups hash every artifact to check integrity. Checks receive
private hard links (copies across filesystems), so eviction cannot remove a
live check's files. When a run-local reference artifact becomes an input,
its hard link is detached into a private copy before stamping: later cache links or eviction
must not change a watched inode's ctime. Checks must never modify reference
artifacts in place.

`--fresh-reference` bypasses hits and replaces completed entries.
`--reference-budget BYTES` sets the maximum retained logical artifact bytes
(default 20 GiB); the newest used entries that fit are retained after a
normally concluded run. Zero retains none. A separate `used` file records
recency. Cache storage or eviction failures are reported; eviction failures
do not change the verification verdict.

`brokkr run pbfhogg-verify -- clean-references` removes completed entries and
old staging/trash without needing a pbfhogg executable, dataset or reference
tool. It uses the same config and scratch selection as verification.

## Results

Each check prints one line on stdout: `NAME: PASS (time; steps)`, optionally
with the claim a PASS establishes, or `FAIL`, `ENVIRONMENT`, `INTERRUPTED`.
Detail is replayed only for a check that did not pass, or streamed with
`--verbose`. A comparison failure recorded during a check is its verdict even
if a later step ends it with an environment error. `all` runs every check in
suite order, continues past comparison failures
and environment outcomes, and stops on an interrupt or verifier error. Its
banner counts passed, failed, environment and skipped entries separately.
A verifier error prints `ERROR`, retains the directory and exits 2. Malformed
reports or contradictory comparison accounting are explicit check failures;
configuration, unsupported comparison domains and infrastructure errors are
verifier errors. An unsettled tree also stops with exit 2.

| Exit | Meaning |
|---|---|
| 0 | every check passed |
| 1 | at least one check failed: pbfhogg disagreed or failed a run |
| 75 | no failure, but at least one check could not be completed (ENVIRONMENT) |
| 130 | interrupted |
| 2 | the verifier could not do its own work (configuration, missing tool, unsettled launch) |

A reference tool's own failure (a non-accepted exit) fails the check, as in
brokkr; a reference tool killed by SIGKILL, which the verifier never sends
while it waits, is ENVIRONMENT.

Deliberate legacy exit difference: brokkr reports runtime configuration refusals and
its catch-all check errors as exit 1. This verifier reserves exit 1 for an
explicit comparison failure and exits 2 when it cannot do its own work.
For example, `all --dataset germany` refuses before checking anything when
several OSCs are configured and none is selected: exit 2 here, exit 1 in
brokkr. Select the same `--osc-seq` on both tools to compare their suite
verdicts. Missing tools, infrastructure errors and unsupported comparison
domains follow the same exit-2 rule; malformed comparison reports remain
FAIL. This distinction prevents a configuration problem from being reported
as a pbfhogg defect. Unlike the legacy catch-all, a verifier error stops the
suite because a later comparison cannot make that run complete.


## Checks

| Check | Compared against | Established by |
|---|---|---|
| `field-policies` | osmium decodings of one-field edits of an osmium-built slice of the input | each policy reads exactly the field classes it claims, with every edit isolated by an independent decoding; `between` by declaration (see How outputs are judged) |
| `sort` | `osmium sort -s multipass` | element equality, and the output's strict order from its OPL decoding, never the header claim |
| `cat` | `osmium cat` | node, way and relation outputs: element equality, strict order and one-way sort-feature comparison |
| `cat --dedupe` (command `cat-dedupe`) | the input itself, osmium OPL decodings | self-merge, merge with an osmium-derived subset, the disjoint union of that subset and its complement in both orders, and merges with a window of the input's own blobs in both orders: element equality with the input, blob layout and summary counts against the plan predicted from both framings, sort claim and `LocationsOnWays` header rule; Unsorted refusal on claimless input |
| `tags-filter` | `osmium tags-filter -R` | three expressions with omitted references, same content/order checks as cat |
| `getparents` | `osmium getparents` | separate node, way, relation and mixed requests, with and without add-self; selection, content and tags by ID, with absent witnesses named |
| `add-locations-to-ways` | `osmium add-locations-to-ways --keep-member-nodes` | explicit sparse/external engines; element equality with inline coordinates on both routes, sort declaration, own inline-location agreement, and engine parity (element equality and full-record digest) under mode all |
| `inspect-tags` | `osmium tags-count` | every kind and `-t way`: count and key rows in order, each tool's key quoting decoded |
| `getid-removeid` (alias `getid`) | `osmium getid`, `removeid`, `getid -r` and `getid -r -t` | endpoint selection by kind, invert element equality with and without way IDs (inline coordinates on a `LocationsOnWays` input without them) and count complement, recursive referenced content and tag removal, with absent witnesses named |
| `extract` | `osmium extract` | strict order, element content and per-strategy membership on the bbox and every configured polygon; simple relations follow the any-direct-node-or-way rule judged against complete_ways |
| `multi-extract` | sequential pbfhogg extracts | simple config output per region has strict order and element equality with its sequential extract (single-pass/sequential parity), for strips alone and, when configured, strips plus polygons |
| `geocode` | a verifier-owned model of the documented query rules over `osmium tags-filter` and `osmium add-locations-to-ways` OPL (one decoding of address, highway and interpolation objects, one of boundary relations) | `geocode-query` over two `build-geocode-index` builds of the input, at `--admin-tolerance 20` and `0`, on seeded uniform and witness queries aiming at a quota per class within the fine radius, between the radii and beyond both (at least one each is enforced for every class the model holds), plus witnesses near boundary vertices: every judged address, street and interpolation answer is one of the nearest candidates (tied within 0.1 mm) with equal text and address coordinates, distance within 0.1 mm, a street's nearest point within two decimicrodegrees and an interpolation number one the documented range rules give within the reach of the nearest point's truncation, or none beyond the coarse radius, every answer key present; an input with no answering range judges interpolation absence only and says so. Every judged admin level is a containing area of least stored area (the area within a float error bound of the exact outer-ring shoelace, and no larger than any other containing area's bound), or none; a level is not judged within 1 mm (tolerance 0) or 21.2 m (tolerance 20) of a boundary of that level, or inside the box of a relation the model leaves unmodelled (a member endpoint shared by three or more member ends, a one-node member, an unlocated member node, a hole nearer than a decimicrodegree to an outer); at least one containing answer per build is enforced. Queries within 0.1 mm of the coarse radius are not judged; a modelled way with an unlocated node, data beyond 85 degrees of latitude or near the antimeridian, or a segment spanning half a turn of longitude is a verifier error; a tag value holding a NUL is outside the comparison (libosmium cuts it) |
| `check-refs` | `osmium check-refs` | missing counts per class in ways-only and relation modes, each exit held to its complete report |
| `check-ids` | `osmium fileinfo -e` and independent framing reader | plain/full verdict, exit, counts and indexed status on input, derived disorder and five fixed fixtures |
| `repack` | osmium OPL decodings, walked in file order | element sequence including metadata and way coordinates, header features, true indexdata, caps and documented blob layout |
| `degrade` | osmium OPL decodings, walked in file order | retained elements, removals and swaps, header features, true indexdata, caps, layout predictions and swap positions; way coordinates stripped on a `LocationsOnWays` input (`--locations-on-ways`, suite `degrade -l`) |
| `apply-changes` | `osmium apply-changes` | strict output order and element equality, tolerating only osmium-only identities the input OSC finally deletes, and no finally deleted identity in pbfhogg's output; optional inline-location consistency |
| `merge-changes` | independent positional model and `osmium merge-changes -s` | concatenation, positional simplify, first version regression refusal, delete bodies and serial parity |
| `tags-filter --input-kind osc` | `osmium tags-filter -R` on input OPL | selected creates/modifies, every delete, grouped input order, action/metadata/body records and serial parity |
| `diff` | osmium OPL decodings of both inputs, merged | every text record and detail, four summaries/exits, indexed/unindexed and plain/verbose parity |
| `diff --format osc` | osmium derive-changes and apply-changes | content-only element equality of roundtrips, pbfhogg's OSC applied by both tools, and indexed/unindexed decompressed OSC parity |
| `renumber` | osmium renumber | every object paired at its renumbered ID; every difference is an orphan reference named past the live range on both sides, one to one |

Apply-changes accepts `--osc-seq` or a fixture `--osc`, and
`--locations-on-ways`. A configured OSC is required. A base without the
optional sort feature is held to exit 2 with the exact Unsorted advice and
no published output; references do not run. In locations mode that base
must still declare LocationsOnWays, so the refusal has only one cause.
On sorted input, pbfhogg's output must be strictly ordered. An identity
(kind and ID) is finally deleted when its last entry in the input OSC, in
file order, is a delete; the verifier reads that from the input itself,
strictly. No finally deleted identity may appear in pbfhogg's output, even
where osmium's output holds it too. The element diff tolerates only
osmium-only identities that are finally deleted, since osmium resolves a
delete by version order against the base, keeping a base element whose
version is above the delete's, while pbfhogg applies the last change
whatever the base's version. All other differences fail. Both outputs must
be in strict order for the element comparison to be made.
In locations mode pbfhogg's output must pass `check --locations`;
osmium's location report is informational, with its stale count shown even
on PASS. The element comparison does not read inline coordinates in that
mode, since osmium's are known stale.
The suite runs the locations arm separately on the selected generation's
`altw-keep-untagged` base, when both it and an OSC are configured, and
skips it for a fixture PBF. Configured invalid files fail resolution.

Cat --dedupe has no reference tool: osmium merge keeps the later timestamp
among copies of one type, ID and version where pbfhogg keeps the earliest
input's, and refuses repeats inside one input (DEVIATIONS.md). The check
works where the survivor cannot matter: every merged operand is the input
or an osmium `tags-filter -R` selection of it (`nwr/amenity`, and its
`--invert-match` complement), so equal keys decode to equal records and the
answer is the input. On input claiming the sort order, which must also be
strictly ordered as osmium decodes it (else a verifier error), each of
`dedupe(A, A)`, `dedupe(A, S)`, `dedupe(S, C)` and `dedupe(C, S)` must equal
the input under `Policy::between`, claim the sort order, and keep or drop
`LocationsOnWays` as documented. A window operand W, `osmium getid` of
every second ID of the tail half of one interior node blob of A and the
head half of the next, and of one interior way and relation blob, adds
`dedupe(A, W)` and `dedupe(W, A)`. The derived operands carry no indexdata,
so their runs pass `--force`; each must be strictly ordered and hold an
object, else a verifier error.

Every merge is also held to the blob layout the documented plan
(docs/cli/commands.md, cat `--dedupe`) predicts from both inputs' framing as
`pbf_frames` decodes it (`checks/dedupe_plan.rs`): each passthrough blob's
payload bytes in place, each run's re-encoded blobs wholly within its ID
span and ascending, nothing after the last segment; the summary's blob
counts equal the prediction (pbfhogg's own account, beside the layout); and
`LocationsOnWays` is dropped exactly when a way blob is in a run. The
prediction's domain is every blob nonempty, single-kind, one element field
per group, strictly ascending non-negative IDs, and indexdata, where
present, equal to the decode; else a verifier error. The window must yield
one run flanked by same-kind passthroughs per windowed kind, the node run
joining two A blobs, else a verifier error. Byte equality witnesses the
preserved payload, not that nothing decoded it. The survivor rule among
differing copies is not established; only `crates/cli/tests/cli_cat.rs`
fixtures cover it. Claimless input is held to the Unsorted refusal of `dedupe(A, A)`,
with `--force` on unindexed input.

No other tool runs beside osmium. osmosis, which deletes regardless of
version, is not run: deletion is enforced from the input OSC's final
deletes above, and its output only ever fed a printed element-count line,
which proves no element identity. Nothing is downloaded or provisioned.
osmconvert is not a reference: its PBF reader refuses BlobHeaders that carry
indexdata, so it cannot read the indexed input the check runs on (some of
those refusals exit 0 with an empty output).

Cat, tags-filter and getparents use the input's optional sort feature to
route comparisons. Without it, osmium reads a sorted copy, pbfhogg outputs
must carry no sort claim and are compared through sorted copies whose
strict order excludes repeated IDs. With it, outputs must be strictly
ordered and a reference sort claim requires pbfhogg's claim. Getparents
refuses negative IDs and negative references osmium could fold.

Check-refs reads pbfhogg's original input and osmium's sorted copy when
needed. It accepts osmium exit 1 only with the complete final summary and
requires empty stdout. It does not accept `--direct-io`.

Inspect-tags runs `pbfhogg inspect tags` and `osmium tags-count` on the
input, over every kind and over ways alone. Both reports are parsed whole
into rows of decimal count, tab, one quoted key and newline, any other byte
failing the comparison; osmium's doubled-quote quoting (a tab or newline in
a key written literally) and pbfhogg's JSON string literal are each decoded,
and the row lists must be equal in order, which DEVIATIONS.md promises. A
pbfhogg key holding a NUL makes the check an error, not a verdict, since
libosmium cannot count such a key faithfully.

Add-locations-to-ways accepts `--mode sparse|external|all` (default all).
On input without the optional sort feature, external must refuse with exit
2, one Unsorted advice naming the input, and no published output. External
alone runs no osmium comparison there. Sparse follows the unsorted content
comparison route; all names both routes and omits engine parity there.
Every selected engine runs before output checks, and a failed engine is
recorded while the other still runs. osmium's output and each engine's
must declare `LocationsOnWays`, so each engine's output is held to it by element
equality with every inline way coordinate at 1e-7 degrees, a dropped node's
included: on sorted input the output itself, on unsorted input its
osmium-sorted copy, which keeps the coordinates. Agreement
(`check --locations --agreement`) checks kept node records and repeated
references. Parity holds the engines to each other by element equality and
by `digest --form full`, which also covers what osmium's records do not
represent, ignoring the header digest. `--inject-prepass` is refused before any check launches: enriched output is
osmium-incompatible.

Getid-removeid requires a non-history snapshot with true indexdata claims
and unique records by kind and ID. An input claiming a negative ID is
refused before any run, since osmium removeid and getid -r match an ID by
its absolute value. Include/invert requests take the lowest and highest
claimed IDs per kind. Each selected count must match its request, and
selected plus inverted counts must match the input; each invert output is
compared with osmium removeid of the same IDs. A second invert request
takes only the node and relation endpoints, so its output must keep a
`LocationsOnWays` declaration; its PASS names it absent when the input
claims ways but no node or relation. The referenced request separately
chooses a relation, highest way and node; its PASS names missing
recursion, member and tag witnesses. Negative references that osmium could
fold are refused. Without the optional sort feature, comparisons use the
same sorted-copy route as cat.

Extract accepts `--bbox` (otherwise the dataset's bbox), and runs simple,
complete_ways and smart on that box and every dataset polygon in name order.
At least one region is required. Smart uses osmium's
`-S types=multipolygon,boundary`. Complete and smart require identical
membership; simple requires identical nodes and ways, and relations holding
any direct node or way member in those sets. Relations present only in
pbfhogg simple must have their first node or way member outside the sets,
which explains osmium simple's omission. Shared element content must agree.
A polygon with multiple features, an open ring or an edge spanning more
than 180 degrees is refused before launching either tool, since their
interpretations differ. On input without the optional sort feature,
osmium extracts a sorted copy; pbfhogg output must have no sort claim and
is compared through a strictly ordered sorted copy. PASS names the regions
and membership differences, including the simple relation deviation.

Multi-extract needs a bbox and accepts `--regions` (default five, positive)
for adjacent longitude strips. Its sequential simple extracts run buffered;
`--direct-io` reaches config runs only. With polygons it also runs a mixed
strip/polygon config. This establishes neither osmium membership nor that
the single-pass path ran. Without the optional sort feature pbfhogg uses
its sequential fallback: neither output may claim sorted, and comparisons
use sorted copies with strict order checks. The PASS line states that no
single-pass route was exercised. Config outputs receive private staging;
config and polygon paths are declared reads, and selected polygon files
are watched like PBF inputs.

`check-ids` runs `pbfhogg check --ids` and `check --ids --full`, both
`--json` with the report written to the check's directory. osmium has no ID
check, so the expectation comes from two readings of each file: `osmium
fileinfo -e -j --no-crc` (`verify_fileinfo`), whose `objects_ordered` and
`multiple_versions` say whether the decoded objects strictly ascend by kind
and then ID in canonical order (libosmium's `id_order` is pbfhogg's), and
the verifier's header-only reading of the framing (`pbf_frames::framing`), since on
an indexed file pbfhogg judges type order from the kind every data blob
claims, an empty blob's included. A file passes when osmium reads it in
strict order and, if indexed, its claims never step back a kind. An ordered
report without `multiple_versions` is an error, never read as false. Each
mode must exit as its report says (0 with a pass and no violation, 1 with a
failure and at least one), answer as expected, count each kind as osmium
does (what catches a reader that skips objects), and report `indexed`
exactly when the framing carries indexdata. The violations themselves and
`--full`'s duplicate detail are not compared, and the PASS line says so. The
cases: the input; when osmium reads it in strict order, three copies the
check derives, the input stripped of indexdata (when it has any), `degrade
--unsort --elements-per-blob 8000` (a step down across blobs, indexed) and
`degrade --unsort-intra --strip-indexdata` (one inside a blob, unindexed),
each required to keep the input's counts, its framing, and to be out of
order exactly when its degradation fires on those counts (a kind above the
cap, a kind with two objects), else the check is an error; and five fixed
fixtures, written as OPL and converted by `osmium cat`, then rewritten by
`pbfhogg cat` for an indexed form: a way before its nodes and an adjacent
repeated ID in both forms, and an ordered file with an empty blob claiming
nodes appended after its way blob, which osmium reads as ordered and pbfhogg
must fail. Each fixture must show the property it exists for, read from
osmium and the framing. The PASS line names each kind of case, indexed or
not and in order or not, that neither the input nor a copy derived from it
gave (an input osmium reads out of order derives nothing, and neither does
one with no object); the fixtures do not count toward it. An input with
a negative ID is refused (pbfhogg rejects negative input IDs project-wide,
and `--full` reports each as untrackable), and one whose framing mixes blobs
with and without indexdata too. Kind claims must be true: a blob whose
decoded kind contradicts its claim stops pbfhogg's check without an answer,
an error here.

`repack` and `degrade` hold pbfhogg's two re-encoders to
osmium's reading of the input and to their documented block layout
(`verify_reencode`, shared). Elements: osmium decodes a reference and the
output to OPL with metadata and way coordinates
(`opl,locations_on_ways=true`), and the verifier walks the two record
streams in file order as osmium diff walks its inputs, advancing the side
whose key is lower and pairing equal keys, but comparing whole lines byte
for byte where osmium diff compares a checksum that feeds tags without
lengths (`opl_compare::compare_sequence`). No unmatched or differing
record means the two sequences are identical element by element, whatever
their order, a shifted tag boundary (`a=bc` against `ab=c`) included; only
reference-side records means the output is a subsequence of the reference.
A moved or removed object is listed with its line. The reference is the input when osmium reads it in
kind and ID order (fileinfo `objects_ordered`), else a stable partition
osmium builds (`osmium cat -t` per kind, then `osmium cat` of the three: each
kind keeps its input order); for degrade it is written without way
coordinates (plain `osmium cat`), since every re-encode drops them. Layout:
each output's indexdata must be on every blob and true: kind, count and
exact ID range by the verifier's own decode of every blob
(`pbf_frames::index_truth`, raw or zlib; any other compression is an error),
bounding box and tag index by `pbfhogg check --index`; then the header-only
claims walk (`pbf_frames::Claims`,
pulling input and output blobs side by side in constant memory) holds the
output's blobs, in order, to the documented rule run over the input's
claims, which on an indexed input must pass both indexdata checks too (else
an error: they feed the prediction). Every output must also claim
`Sort.Type_then_ID` and `LocationsOnWays` as its command documents, hold no
blob over the cap or empty, and keep kinds in order. Both arms read the input
with `osmium fileinfo -e` and refuse a negative ID and a header requiring
`HistoricalInformation` (one object per kind and ID is the domain); `--force`
is passed only on an unindexed input, which also gets no layout prediction
(the PASS line says so). A run pbfhogg refuses as too large for the format
(exit 2 with its CLI's `; lower --elements-per-blob` advice, nothing
published) is named in the PASS line as refused, its representability and
correctness not established; an arm all of whose runs refuse is an error.

`repack` runs the default cap (no `--elements-per-blob`, held to the
documented 8000), 3000 (a cap that does not divide 8000-element blobs) and
64000 (one that grows them), each of which must leave the element sequence
identical, metadata and way coordinates included, and claim both header
features as the input does; on an indexed input the layout must be repack's
rule exactly: per kind, a blob of at least the cap flushes the builder and is
written as full blocks of the cap and its remainder alone, a smaller one goes
element by element into the builder, which flushes at the cap. A fourth case
repacks the input stripped of indexdata (`degrade --strip-indexdata`,
required to be unindexed and to hold the input's data payloads byte for byte,
else an error) with `--force` at the default cap, its layout predicted from
the original's claims. The PASS line names a shape no cap met: a shrink
blob's remainder followed by a smaller blob of its kind (the remainder must
not take it), or a blob smaller than the cap coalescing into a nonempty
builder.

`degrade` runs the decode path at the default cap. `--strip-locations`
must leave the sequence identical and, on an indexed input, re-encode each
input blob alone (full blocks, then the remainder). On an input osmium reads
in strict order with at least four objects, three more run, else the PASS line
says they did not. `--drop-ids N:SEED`, N the smaller of 10000 and a quarter
of the objects, must remove exactly N objects and change nothing else (each a
`-` line alone), its layout predicted from each input blob's survivors (the
removed IDs counted into the blob whose claimed range holds them); it runs
twice with one seed, the outputs identical, and once with another, held to
the same checks. Which objects go is not compared with pbfhogg's selection,
which is not documented, and whether the second seed chose another set is
named, never a verdict. `--unsort` and `--unsort-intra` must move exactly one
object per kind with more objects than the cap (two or more for intra), its
two lines identical, and change nothing else. For `--unsort` each kind must
be packed continuously (full blocks, then the rest), exactly the kind's first
two blobs may overlap, and the verifier decodes three IDs itself
(`pbf_frames::blob_ids`, raw or zlib, its count and kinds held to the claim):
the moved object must open the kind's second blob and fall between the last
two IDs of its first, which holds the cap's worth. With the rest of the
sequence unchanged and ascending, that places the input's objects at the cap
and one past it swapped, and nothing else. For `--unsort-intra` the moved
object must be the kind's lowest ID by osmium and decode second in the kind's
first blob, and no blob ranges may overlap. The unsort modes must not claim
`Sort.Type_then_ID`; the other cases claim it as the input does; none claims
`LocationsOnWays`.

Which input each arm strips coordinates on: the decode path drops inline way
coordinates on every case, whatever the flags, so every case strips them
when the input carries them and none does otherwise. The suite's `degrade`
entry runs on the suite input (by default `indexed`, which declares no
`LocationsOnWays`; the PASS line names the gap). The suite's `degrade -l`
entry runs the whole check on the selected generation's
`altw-keep-untagged` base, and standalone `degrade --locations-on-ways`
does the same on the selected input. On a `LocationsOnWays` input the check
first compares the reference with way coordinates against the one without
(both the input, or both the same stable partition): an unmatched object is
a failure, and no differing record means no way carried a coordinate. In the locations entry an input that declares no
`LocationsOnWays`, or whose ways carry no coordinate, fails; elsewhere it is
a PASS-line gap. That a retained coordinate would be caught rests on
osmium's OPL writing way node coordinates under `locations_on_ways=true`;
the generated fixtures fake osmium and exercise only the wiring.


Change-only checks resolve no PBF. Merge-changes takes `--osc-seq`, repeated
fixture `--osc PATH` in argument order, or inclusive `--osc-range LO..HI`
with every sequence present. OSC tags-filter takes `--osc-seq` or one
fixture `--osc`. Both use the snapshot fallback above and watch every
resolved OSC. They refuse `--direct-io`; use `--osc`, never `--input`, for
fixtures. The suite runs both on its single selected OSC.

Every change listing joins osmium OPL with a strict XML scan by position,
requiring equal kind, ID, version and length. Comparisons hold action labels,
metadata attributes (empty expected attributes must be omitted), tags,
coordinates, references and members record for record; deletes retain their
bodies and an unlocated deleted node remains unlocated. Section boundaries
are not compared. Automatic `.osc.gz` output must have the same decompressed
content as `--jobs 1` plain `.osc` output.

Merge-changes default output equals its concatenated inputs. Simplify keeps
the last change per object by input position, grouped create, modify, delete,
then kind and signed ID. An external sort models histories with bounded
memory. A lower claimed version than an earlier version is a regression;
unversioned changes neither raise nor reset that floor. Both simplify runs
must refuse the first regression by position with exit 2, the object and
both versions in stderr, and no publication. Allow-version-regression must
instead match positional survivors. The model separately predicts osmium's
highest-version, latest-timestamp, latest-position survivors, and checks
osmium against that prediction. This cross-check is omitted, and named in
the claim, for input objects mixing present and absent timestamps. The
inputs, their replay and fixed precedence/regression/delete-body fixtures
run through the production check; each fixture must demonstrate its intended
property before judging. Scratch is declared only for multiple-input,
non-simplifying runs with jobs other than 1.

OSC tags-filter runs four cases: `highway=primary`, `amenity=restaurant`,
`w/highway=primary`, and `n/amenity r/type=route`. Osmium filters the input
OPL with `-R`; the expected listing consumes matches in lockstep, keeping
matching creates/modifies and every delete. Output groups actions then kinds,
keeping each kind's input order. PASS counts selected changes and bodied
deletes, without claiming section-boundary identity.


Diff accepts `--format text|osc` (default text), `--osc-seq` or one fixture
`--osc`. Both formats build the second PBF with buffered apply-changes;
the selected OSC uses the snapshot fallback and is watched. Direct IO
reaches only the diff under test. Without the optional sort feature, both
formats must refuse the input as OLD and as NEW against an osmium-sorted
copy: exit 2, advice naming the input and no publication or text listing.
No content comparison runs on that route.

Text diff runs plain and verbose on the original pair and the pair stripped
of indexdata. Osmium decodes both inputs to OPL with metadata and the
verifier merges them strictly in object order (`opl_compare::compare`,
negative IDs included), listing every one-sided object and every pair whose
line differs; an identity not listed is byte-identical on both sides, so
every identity is judged, untouched ones included. Listing consumption is
reconciled with the merge's counts. Content excludes metadata and inline
way coordinates.
The verbose listing must name exactly the content differences, with the
correct class, ID, versions and complete coordinate, tag, reference and
member detail. Edit scripts must be faithful and shortest within the
1000-edit cap; replacements must name the trimmed ranges. A replacement
within that cap can be accepted on pbfhogg's search budget, which is not
judged. Version zero in OPL admits missing or present version spellings.
PASS names these qualifications and detail shapes not exercised. The
stripped verbose listing must match byte for byte, plain listings must
match its headers, and all summaries and exits must match independent
content counts. Which equally short edit script is chosen is not judged.

OSC diff derives with both tools and applies each result with its
producing tool; osmium also applies pbfhogg's OSC, so what that OSC means
is decided by a tool other than pbfhogg. All three roundtrips are compared
with new, and the two own-tool roundtrips with each other, by content-only
element equality (no metadata, visibility or inline coordinates: pbfhogg's
OSC is content-based by design, and the domain is a non-history snapshot).
New and pbfhogg's roundtrip must be strictly ordered. The unindexed
pair must produce the indexed OSC's exact decompressed bytes, including
metadata; gzip framing is excluded. The roundtrips do not establish
metadata identity or inline coordinates.

Renumber accepts `--start-id`, forwarded to both tools. Direct IO reaches
only pbfhogg renumber. An input without the optional sort feature must
refuse with the exact advice and no output; osmium does not run there.
Otherwise the two outputs are compared by element equality, metadata
included, with no tolerance. Every object must pair by kind and ID, and each
kind's IDs must run `start, start + 1, ...` on both sides, which fixes the
live range `start .. start + count`. No node may differ. A way or relation
may differ only in the ref IDs of its node or member list (same fields,
list length, member types, encoded roles and coordinate suffixes). Every
reference in every record, alike or not, must be the same live ID on both
sides or lie at or above its kind's live range on both, and those orphan
names must correspond one to one per kind across the file. That proves the
outputs the same reference graph up to orphan naming, the documented
deviation; it does not check pbfhogg's shift formula against the input.

## Where pbfhogg is still a comparator

Every verdict above is reached by osmium or the verifier's own readers,
except these, each kept because nothing independent covers it:

- `check --locations` (apply-changes locations mode, and `--agreement` in
  add-locations-to-ways): no osmium command holds inline coordinates to kept
  node records. In add-locations-to-ways the element equality with osmium
  already holds every coordinate, so it is additional evidence there; in
  apply-changes locations mode, where osmium's own coordinates are stale, it
  is the only coordinate evidence.
- `check --index` for bounding boxes and tag indexes, which the verifier's
  blob decode does not read (it holds kind, count and ID range itself).
- `digest --form full` engine parity, kept beside element equality for the
  fields osmium's records do not represent.
- Multi-extract's reference is pbfhogg's own sequential extract: the
  comparison is independent, the expectation is not, so it is
  single-pass/sequential parity; `extract` holds extraction to osmium.
- Inputs pbfhogg builds rather than judges: `degrade --strip-indexdata`
  copies (diff, diff --format osc, check-ids, repack), `apply-changes`
  building the diff checks' second PBF, and `cat`/`degrade` deriving
  check-ids fixtures. A defect there changes what is compared, never how.
- Informational lines, which carry no verdict.

## Migration evidence and ownership

The verifier owns pbfhogg cross-validation; elivagar and nidhogg verification
remain in brokkr. Retirement is tracked by brokkr#183. Legacy `merge` is
`apply-changes`, `tags-filter-osc` is `tags-filter --input-kind osc`, and
`derive-changes` is `diff --format osc`. There is no verification in a bench.
Devkit supplies selection primitives; the verifier alone owns snapshot OSC
fallback and suite skip policy, which script benches must not inherit.

On 2026-11-13, host bygg, at commit `5aca6b1c` with the uncommitted migration
and CLI xxh3-128 `629ff3c2f5aba05bcd35ee0e953c50db`, the complete new and
legacy suites were compared sequentially, verdict for verdict, on indexed
Denmark and Germany. Denmark: 19 PASS each, exit 0, no skips. Germany with
`--osc-seq 4705`: 18 PASS and sort ENVIRONMENT each, exit 75, no skips.
There were no per-check verdict differences. Both fresh Germany sort
references were killed by SIGKILL, which neither verifier sent; the
2026-11-15 sort evidence below identifies the kernel OOM killer. Bare
Germany refused ambiguous selection among eight OSCs before checking
anything, with the deliberate configuration-exit difference described above.

On 2026-11-15, host bygg, on the uncommitted change that moved every
pbfhogg-judged comparison to osmium decodings and the verifier's readers
(parent `c757c984`): Denmark 20 PASS, exit 0. Germany with `--osc-seq
4705`: 18 PASS and sort ENVIRONMENT (osmium sort SIGKILLed again), diff
failing only on scratch exhausted by cached OPL decodings; with decodings
uncached and removed once merged, diff then passed alone.

On 2026-11-15, host bygg (30.5 GiB RAM, 31 GiB swap), at `95232102`:
`sort --dataset germany --fresh-reference` with the simple strategy was
SIGKILLed after osmium sort ran 4m11s, then passed on a rerun with osmium
sort at 9m54s, swap-bound. The kernel's `/proc/vmstat` `oom_kill` count
since boot equalled the number of germany osmium-sort SIGKILLs recorded
since boot, so the killer is the OOM killer. On the uncommitted change to
`-s multipass`, three fresh runs passed with osmium sort at 2m14s, 2m12s
and 1m46s; the last, sampled, peaked at a resident set (VmHWM) of 26.2 GiB. That
peak sits at the edge of the project's 26 GB free-memory host assumption:
germany sort fits bygg, not every conceivable standard host.

On 2026-11-16, host bygg, parent `a4a6974a` with the uncommitted change that
added `cat-dedupe` (CLI xxh3-128 `a9b2163354aa448a02f23aebab86ed8e`): Denmark
indexed, raw and `altw-keep-untagged` PASS (1m48s to 1m53s), the `unsorted`
snapshot PASS as a refusal, and Germany indexed PASS in 17m21s.

On 2026-11-16, host bygg, parent `fbfe8a26` with the uncommitted change that
added the blob-layout prediction and the window operand to `cat-dedupe`
(CLI xxh3-128 `465714c58738dbf9b612854fb37b1e48`): Denmark indexed, raw
and `altw-keep-untagged` PASS (2m22s to 2m34s; on indexed the window merges
predicted and matched 7392 passthrough and 7 rewritten blobs, three runs
between passthroughs), and Germany indexed PASS in 23m28s.

On 2026-11-16, host bygg, parent `ac310e17` with the uncommitted change that
wrote the cat and removeid references with inline coordinates and decided
the sort and `getid --invert` declarations exactly (CLI xxh3-128
`465714c58738dbf9b612854fb37b1e48`): on Denmark `altw-keep-untagged`, cat
(each type kept), sort (kept, required by disjoint claims on sorted input)
and getid-removeid (invert with way IDs dropped with the warning, invert
without way IDs kept, 59152278 records equal to osmium removeid with
coordinates) PASS; getid-removeid and sort on the `unsorted` snapshot PASS.
No configured input both declares `LocationsOnWays` and has overlapping
way ranges, so sort's required drop is witnessed by unit tests only.

On 2026-11-15, host bygg, on the uncommitted change that added the
locations degrade entry (parent `d5aacbff`): `degrade --locations-on-ways
--variant altw-keep-untagged` on Denmark PASS (every case stripping real
inline coordinates), and plain `degrade` on Denmark indexed PASS with the
no-`LocationsOnWays` gap named.

On 2026-11-15, host bygg, parent `9e9d2e84` with the uncommitted change that
replaced renumber's 10% relation tolerance with the record-by-record orphan
proof (CLI xxh3-128 `5e68c73ce9e09358c32355d6f3552cb1`): indexed Denmark
PASS with 306 differing records, all relations, every one an orphan name
only (distinct orphans referenced: 441 nodes, 32943 ways, 706 relations),
also PASS under `--start-id 1000,2000,3000`; indexed Germany PASS with 6792
differing records, all relations, every one an orphan name only (30849
nodes, 430276 ways, 34731 relations).

The matching locations checks reported informational osmium stale inline
references: Denmark 563 refs (538 nodes), Germany 7986 refs (5346 nodes).
pbfhogg's own locations checks passed. Neither dataset had a configured
polygon on this host, so extract coverage was bbox only. Text diff named
unexercised detail shapes. These are correctness observations, not timing
measurements or stronger claims than the checks specify.

On 2026-11-16, host bygg, parent `adef2fc1` with the uncommitted change
that added `field-policies`: Denmark indexed PASS (769-record slice, all
thirteen controls with a candidate, 64 ways in the `between` coordinate
witness) and Germany indexed PASS. Two mutations of `Policy`'s OPL options
were each caught as `ERROR`, exit 2: CONTENT keeping metadata (five
controls under content) and FULL dropping metadata (ten controls under
full and between).

Every child, including probes, uses `Lifecycle::run`.
Child setup clears the signal mask, installs parent-death SIGKILL and checks
the parent race. Cancellation checkpoints precede launches, verdicts, run
cleanup and exit, and reference publication. Input stamps
are rechecked on error paths too. Cache safety uses the machine lock,
run-unique staging and publication after settlement, never a cache flock.
Dataset eligibility is a constant name allowlist in `main.rs`; widening it
requires the maintainer's decision, not a new dataset config key.
