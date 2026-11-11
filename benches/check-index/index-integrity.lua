-- index-integrity: the typed `brokkr check-index` workload, one
-- `check --index` child per iteration. This selector suppresses the default
-- IDs/refs checks, checking blob indexdata and tagdata against the payload.
-- Dataset, snapshot and variant bind the input for reservation, hashing and
-- pairing. No cache preparation: the typed runner does none either.
-- Child wall matches that workload; iteration wall includes Lua bookkeeping.
--
-- Exit 0 is clean, exit 1 violations, narrowed by expect.check-index when
-- declared. Before 854f9465, only 0 is trusted; the shared expect module
-- holds that policy.
-- A marker stop records a partial run without claiming a full verdict.
-- The marked child supports --hotpath and --alloc, including marker stops.
-- io_uring and compression are absent because typed check-index refuses
-- them. Build features, commit selection and iteration counts belong to the
-- bench runner. Direct-I/O support is built for both read modes.
--
--   brokkr bench --dry-run index-integrity --direct-io yes,no
--   brokkr bench --alloc index-integrity --stop CHECK_INDEX

local expect = require("expect")
local stops = require("stop")
local profiling = require("profiling")

function validate(params)
    stops.resolve(params.stop)
end

function preflight(params)
    if not params.stop then expect.refuse_untrusted(params, "check-index") end
end

function bench(params)
    local options = profiling.options("check-index", params.stop)
    local r = binary_with(options, "check", "--index", params.variant.file, {
        ["direct-io"] = params["direct-io"] == "yes",
    })
    record.summary = r.stderr
    if params.verbose == "yes" then
        print(r.stdout)
        print(r.stderr)
    end
    if r.stopped_at then
        record.stopped_at = r.stopped_at
        return "stopped"
    end
    local verdict = expect.verdict(params, "check-index", "check --index", r)
    record.check_exit = r.exit
    return verdict
end
