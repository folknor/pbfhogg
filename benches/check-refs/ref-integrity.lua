-- ref-integrity: the typed `brokkr check-refs` workload, one `check --refs`
-- child per iteration, with optional relation-member checks and O_DIRECT.
-- Bind dataset, snapshot and variant for input reservation, hashing and
-- pairing. No cache preparation: the typed runner does none either.
-- The child wall matches that workload; iteration wall includes Lua work.
--
-- Exit 0 is clean, exit 1 violations, narrowed by expect.check-refs when
-- declared. Before 854f9465, only 0 is trusted. Like id-order, since()
-- uses ancestry rather than the typed runner's binary capability probe;
-- a backport of the exit split outside that ancestry can differ.
-- A marker stop records a partial run, without claiming a full verdict.
-- The marked child supports --hotpath and --alloc, including marker stops.
-- Build features belong in the registry. Direct-I/O support is built for
-- both read modes.
--
--   brokkr bench --dry-run ref-integrity --check-relations yes,no
--   brokkr bench --hotpath ref-integrity --direct-io yes

local function expected(params)
    local declared = params.variant.expect
    if declared then return declared["check-refs"] end
end

local stops = require("stop")
local profiling = require("profiling")

function validate(params)
    stops.resolve(params.stop)
end

function preflight(params)
    if not params.stop and expected(params) == "violations" and not since("854f9465") then
        fail("declared violations cannot be trusted before the exit-code split (854f9465)")
    end
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
    if r.exit ~= 0 and (r.exit ~= 1 or not since("854f9465")) then
        fail("check --refs ended with exit " .. tostring(r.exit)
            .. ", signal " .. tostring(r.signal) .. ": " .. r.stderr)
    end
    local verdict = "clean"
    if r.exit == 1 then verdict = "violations" end
    local want = expected(params)
    if want and verdict ~= want then
        fail("check --refs answered " .. verdict .. ", input expect.check-refs declares " .. want)
    end
    record.check_exit = r.exit
    record.summary = r.stderr
    return verdict
end
