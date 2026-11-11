-- ref-integrity: the typed `brokkr check-refs` workload, one `check --refs`
-- child per iteration, with optional relation-member checks and O_DIRECT.
-- Bind dataset, snapshot and variant for input reservation, hashing and
-- pairing. No cache preparation: the typed runner does none either.
-- The child wall matches that workload; iteration wall includes Lua work.
--
-- Exit 0 is clean, exit 1 violations, narrowed by expect.check-refs for a
-- plain run and expect.check-refs-relations under --check-relations, never
-- one for the other: clean way refs say nothing about relation members,
-- which an extract cuts at its boundary. Before 854f9465, only 0 is
-- trusted; the shared expect module holds that policy.
-- A marker stop records a partial run, without claiming a full verdict.
-- The marked child supports --hotpath and --alloc, including marker stops.
-- Build features belong in the registry. Direct-I/O support is built for
-- both read modes.
--
--   brokkr bench --dry-run ref-integrity --check-relations yes,no
--   brokkr bench --hotpath ref-integrity --direct-io yes

local function key(params)
    if params["check-relations"] == "yes" then return "check-refs-relations" end
    return "check-refs"
end

local expect = require("expect")
local stops = require("stop")
local profiling = require("profiling")

function validate(params)
    stops.resolve(params.stop)
end

function preflight(params)
    if not params.stop then expect.refuse_untrusted(params, key(params)) end
end

function bench(params)
    local options = profiling.options("check-refs", params.stop)
    local r = binary_with(options, "check", "--refs", params.variant.file, {
        ["check-relations"] = params["check-relations"] == "yes",
        ["direct-io"] = params["direct-io"] == "yes",
    })
    if r.stopped_at then
        record.stopped_at = r.stopped_at
        return "stopped"
    end
    local verdict = expect.verdict(params, key(params), "check --refs", r)
    record.check_exit = r.exit
    record.summary = r.stderr
    return verdict
end
