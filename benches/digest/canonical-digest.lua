-- canonical-digest: the typed `brokkr digest` workload, one child per
-- iteration. Dataset, snapshot and variant bind the input for reservation,
-- hashing and pairing. No cache preparation: the typed runner does none.
-- Child wall matches that workload; iteration wall includes Lua bookkeeping.
--
-- form=auto matches omission in the typed runner: explicit full from
-- 16154b0d onward, implicit content before it. Explicit content omits the
-- flag on older builds; explicit full is always forwarded, so they refuse it.
-- since() follows commit ancestry, so a backport outside it can differ from
-- the typed runner's capability probe. The resolved form is recorded and
-- returned as the verdict; use an explicit form for matched comparisons
-- across the transition, since auto binds the same param on both sides.
-- Digest's stdout report and stderr are recorded. Nonzero exits fail.
-- Marker stops record partial runs without claiming a complete digest.
-- The marked child supports --hotpath and --alloc, including marker stops.
-- io_uring and compression are absent because the typed command refuses
-- them. Features, commit selection and iteration counts belong to the runner.
--
--   brokkr bench --dry-run canonical-digest --form auto,full,content
--   brokkr bench --alloc canonical-digest --form content --direct-io yes

local stops = require("stop")
local profiling = require("profiling")

function validate(params)
    stops.resolve(params.stop)
end

function bench(params)
    local takes_form = since("16154b0d")
    local form = params.form
    if form == "auto" then
        if takes_form then form = "full" else form = "content" end
    end
    local flag = form
    if form == "content" and not takes_form then flag = nil end
    local options = profiling.options("digest", params.stop)
    local r = binary_with(options, "digest", params.variant.file, {
        form = flag,
        ["direct-io"] = params["direct-io"] == "yes",
    }):ok()
    record.form = form
    record.digest = r.stdout
    record.summary = r.stderr
    if params.verbose == "yes" then
        print(r.stdout)
        print(r.stderr)
    end
    if r.stopped_at then
        record.stopped_at = r.stopped_at
        return "stopped"
    end
    return form
end
