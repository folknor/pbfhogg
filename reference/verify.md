# pbfhogg-verify: cross-validation contract

`pbfhogg-verify` (`crates/verify`) cross-validates the pbfhogg CLI against
reference tools: osmium for every check, osmosis and osmconvert where a check
asks them. It replaces pbfhogg's part of `brokkr verify`, with the complete
19-check suite available here.

```sh
brokkr run --with pbfhogg pbfhogg-verify -- sort --dataset denmark
brokkr run --with pbfhogg --with-commit REF pbfhogg-verify -- all --dataset germany
```

`--with-features LIST` builds the CLI under test with features (for
`--direct-io`). The verifier never calls brokkr and links no pbfhogg: no
verdict may rest on code compiled into the verifier.

## Inputs

- `--dataset` (default `denmark`), `--variant` (default `indexed`),
  `--snapshot KEY` resolve through `pbfhogg-devkit` from the composed
  `brokkr.toml`, pins checked.
- Real-data verification runs on denmark and germany only. Every
  dataset-backed input is refused for any other dataset; planet and the
  continents are bench-only. The policy is keyed on the dataset name, never
  inferred from file size.
- `--input PATH` names a fixture PBF instead (relative to the project root).
  It exempts only that PBF, never a dataset-resolved OSC or base.

The `all` command also accepts `--osc-seq SEQ`, `--osc PATH` and
`--bbox=WEST,SOUTH,EAST,NORTH`. `--osc` and `--osc-seq` conflict. An explicit
fixture OSC exempts only itself from dataset eligibility. Bboxes require
four finite coordinates and preserve their original spelling. Only `all`,
`extract` and `multi-extract` accept `--bbox`; change-only selection never
implies geographic filtering.

The suite reports all 19 entries in order, including explicit `SKIPPED`
lines counted separately from passes. Extract skips without either a bbox
or polygons; multi-extract skips without a bbox. The six OSC-consuming
entries skip only when no OSC is configured. The locations apply-changes
entry also skips for a fixture PBF or an absent selected-generation
`altw-keep-untagged` base. Configured invalid inputs fail resolution, even
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
or verdict is cached: every comparison runs again. A hit is shown as
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
| `sort` | `osmium sort` | `pbfhogg diff --suppress-common` empty, and the output's computed element order (`inspect --extended` `data.objects_ordered`), never the header claim |
| `cat` | `osmium cat` | node, way and relation outputs: element diff, strict order and one-way sort-feature comparison |
| `tags-filter` | `osmium tags-filter -R` | three expressions with omitted references, same content/order checks as cat |
| `getparents` | `osmium getparents` | separate node, way, relation and mixed requests, with and without add-self; selection, content and tags by ID, with absent witnesses named |
| `add-locations-to-ways` | `osmium add-locations-to-ways --keep-member-nodes` | explicit sparse/external engines; content and sort declaration, own inline-location agreement, and full-record engine parity under mode all |
| `getid-removeid` (alias `getid`) | `osmium getid`, `getid -r` and `getid -r -t` | endpoint selection by kind, invert count complement, recursive referenced content and tag removal, with absent witnesses named |
| `extract` | `osmium extract` | strict order, element content and per-strategy membership on the bbox and every configured polygon; simple relations follow the any-direct-node-or-way rule judged against complete_ways |
| `multi-extract` | sequential pbfhogg extracts | simple config output per region has strict order and identical element content, for strips alone and, when configured, strips plus polygons |
| `check-refs` | `osmium check-refs` | missing counts per class in ways-only and relation modes, each exit held to its complete report |
| `check-ids` | `osmium fileinfo -e` and independent framing reader | plain/full verdict, exit, counts and indexed status on input, derived disorder and five fixed fixtures |
| `repack` | `osmium diff` | element sequence including metadata and way coordinates, header features, true indexdata, caps and documented blob layout |
| `degrade` | `osmium diff` | retained elements, removals and swaps, header features, true indexdata, caps, layout predictions and swap positions |
| `apply-changes` | `osmium apply-changes` | strict output order and element content, tolerating only osmium-only IDs in the input OSC delete set; optional inline-location consistency |
| `merge-changes` | independent positional model and `osmium merge-changes -s` | concatenation, positional simplify, first version regression refusal, delete bodies and serial parity |
| `tags-filter --input-kind osc` | `osmium tags-filter -R` on input OPL | selected creates/modifies, every delete, grouped input order, action/metadata/body records and serial parity |
| `diff` | osmium diff OPL and fetched objects | every text record and detail, four summaries/exits, indexed/unindexed and plain/verbose parity |
| `diff --format osc` | osmium derive-changes and apply-changes | element-content roundtrips and indexed/unindexed decompressed OSC parity |
| `renumber` | osmium renumber | equal paired counts, no node or way content differences, classified relation differences at most 10% of relations |

Apply-changes accepts `--osc-seq` or a fixture `--osc`, and
`--locations-on-ways`. A configured OSC is required. A base without the
optional sort feature is held to exit 2 with the exact Unsorted advice and
no published output; references do not run. In locations mode that base
must still declare LocationsOnWays, so the refusal has only one cause.
On sorted input, pbfhogg's output must be strictly ordered. The element diff
tolerates only osmium-only IDs the input OSC deleted, since osmium uses
version-based deletion and pbfhogg deletes unconditionally. All other
differences fail. Osmium's output is normalised with buffered
`pbfhogg sort --force` before the merge-join diff.
In locations mode pbfhogg's output must pass `check --locations`;
osmium's location report is informational, with its stale count shown even
on PASS. The element comparison does not read inline coordinates.
The suite runs the locations arm separately on the selected generation's
`altw-keep-untagged` base, when both it and an OSC are configured, and
skips it for a fixture PBF. Configured invalid files fail resolution.

Plain apply-changes also runs osmosis and, if found on the caller's PATH,
osmconvert for context only. Their failure or unavailability does not
change the comparison verdict; interruption and unsettled children still
stop the run. Neither tool runs in locations mode.
Osmosis 0.49.2 and its pinned Temurin JDK are downloaded by curl with only
the boundary's proxy/CA settings added to the private launch environment.
SHA-256 is checked in-process before unpacking. Each launch uses the same
lifecycle owner. The completed tree, including its marker and JDK, is
published in one rename to `<data>/osmosis-0.49.2`, after a cancellation
checkpoint. An existing directory without the exact completion marker is
refused, never reused. Failed staging trees remain under a unique
`<data>/.osmosis-RUNID` for diagnosis. Installation is independent of
brokkr's older unversioned tool directories.

Osmosis reference identity includes its complete install and Java trees,
plus the libraries ldd resolves for java and libjvm.so. Its environment
sets JAVA_HOME and JAVA_OPTS with launch-private user.home and
java.io.tmpdir. As in the old verifier, a staging path with whitespace,
quotes or backslashes omits those two properties and disables caching,
because the launcher splits JAVA_OPTS with xargs.

Cat, tags-filter and getparents use the input's optional sort feature to
route comparisons. Without it, osmium reads a sorted copy, pbfhogg outputs
must carry no sort claim and are compared through sorted copies whose
strict order excludes repeated IDs. With it, outputs must be strictly
ordered and a reference sort claim requires pbfhogg's claim. Getparents
refuses negative IDs and negative references osmium could fold.

Check-refs reads pbfhogg's original input and osmium's sorted copy when
needed. It accepts osmium exit 1 only with the complete final summary and
requires empty stdout. It does not accept `--direct-io`.

Add-locations-to-ways accepts `--mode sparse|external|all` (default all).
On input without the optional sort feature, external must refuse with exit
2, one Unsorted advice naming the input, and no published output. External
alone runs no osmium comparison there. Sparse follows the unsorted content
comparison route; all names both routes and omits engine parity there.
Every selected engine runs before output checks, and a failed engine is
recorded while the other still runs. Inline coordinates are never compared
with osmium: agreement checks kept node records and repeated references,
and parity compares the engines' full element records, ignoring the header
digest. Both engines could share a wrong coordinate for a dropped node.
`--inject-prepass` is refused before any check launches: enriched output is
osmium-incompatible.

Getid-removeid requires a non-history snapshot with true indexdata claims
and unique records by kind and ID. Include/invert requests take the lowest
and highest non-negative claimed IDs per kind. Each selected count must
match its request, and selected plus inverted counts must match the input.
The referenced request separately chooses a relation, highest way and node;
its PASS names missing recursion, member and tag witnesses. Negative IDs or
references that osmium could fold are refused. Without the optional sort
feature, comparisons use the same sorted-copy route as cat; invert is
counted only.

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
(`verify_reencode`, shared). Elements: `osmium diff --suppress-common
--output-format opl,locations_on_ways=true --summary` of a reference against
the output. osmium walks both files as a merge on object order and counts a
pair same only when a checksum over visible, timestamp, tags, changeset, uid,
user and the location, way node references with their coordinates or members
agrees, so no left, right or different object means the two sequences are
identical element by element, whatever their order (barring a checksum
collision; tags are fed without lengths). An object it lists as moved or
removed is printed, not checksummed, so its lines are compared byte for byte,
way coordinates included. The reference is the input when its kinds are
grouped in order (`inspect`'s `ordering.standard`), else a stable partition
osmium builds (`osmium cat -t` per kind, then `osmium cat` of the three: each
kind keeps its input order); for degrade it is written without way
coordinates (plain `osmium cat`), since every re-encode drops them. Layout:
each output's indexdata must be on every blob and true by `pbfhogg check
--index`, which decodes every blob independently of the writer's
`BlockBuilder`; then the header-only claims walk (`pbf_frames::Claims`,
pulling input and output blobs side by side in constant memory) holds the
output's blobs, in order, to the documented rule run over the input's
claims, which on an indexed input must pass `check --index` too (else an
error: they feed the prediction). Every output must also claim
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
of indexdata. Osmium's OPL diff supplies one-sided and checksum-different
objects. Every OSC-touched or pbfhogg-reported modified identity absent from
that listing is fetched from both inputs: this catches content changes
osmium's checksum misses. Fetch completion, missing counts, object order,
listing consumption and osmium's summary are reconciled. A candidate found
on only one side, or a negative ID requiring an absolute-ID fetch, refuses
the comparison. Content excludes metadata and inline way coordinates.
The verbose listing must name exactly the content differences, with the
correct class, ID, versions and complete coordinate, tag, reference and
member detail. Edit scripts must be faithful and shortest within the
1000-edit cap; replacements must name the trimmed ranges. A replacement
within that cap can be accepted on pbfhogg's search budget, which is not
judged. Version zero in OPL admits missing or present version spellings.
PASS names these qualifications and detail shapes not exercised. The
stripped verbose listing must match byte for byte, plain listings must
match its headers, and all summaries and exits must match independent
content counts. This assumes apply-changes preserves untouched identities
when osmium's checksum also calls them common; apply-changes has its own
separate check. Which equally short edit script is chosen is not judged.

OSC diff derives with both tools, applies each result with its producing
tool, normalises osmium's roundtrip with buffered pbfhogg sort --force,
and compares both roundtrips with new and with each other using the element
diff. New and pbfhogg's roundtrip must be strictly ordered. The unindexed
pair must produce the indexed OSC's exact decompressed bytes, including
metadata; gzip framing is excluded. The roundtrip comparison itself does
not independently establish metadata identity or inline coordinates.

Renumber accepts `--start-id`, forwarded to both tools. Direct IO reaches
only pbfhogg renumber. An input without the optional sort feature must
refuse with the exact advice and no output; osmium does not run there.
Otherwise the buffered comparison diff must exit 0 or 1 and carry all four
summary fields. Both totals must equal same plus different, excluding
one-sided objects. Classified listing blocks must number exactly different;
no node or way difference is allowed. Relation differences may not exceed
10% of osmium's relation count, the existing tolerance for orphan-reference
handling. Their attribution to orphan handling is not independently proved.

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
references were killed by SIGKILL, which neither verifier sent. Its
comparison remains unestablished; OOM is plausible, not proved. Bare
Germany refused ambiguous selection among eight OSCs before checking
anything, with the deliberate configuration-exit difference described above.

The matching locations checks reported informational osmium stale inline
references: Denmark 563 refs (538 nodes), Germany 7986 refs (5346 nodes).
pbfhogg's own locations checks passed. Neither dataset had a configured
polygon on this host, so extract coverage was bbox only. Text diff named
unexercised detail shapes. These are correctness observations, not timing
measurements or stronger claims than the checks specify.

Every child, including probes and provisioning, uses `Lifecycle::run`.
Child setup clears the signal mask, installs parent-death SIGKILL and checks
the parent race. Cancellation checkpoints precede launches, verdicts, run
cleanup and exit, and reference or installation publication. Input stamps
are rechecked on error paths too. Cache safety uses the machine lock,
run-unique staging and publication after settlement, never a cache flock.
Dataset eligibility is a constant name allowlist in `main.rs`; widening it
requires the maintainer's decision, not a new dataset config key.
