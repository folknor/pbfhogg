-- location-integrity: the typed `brokkr check-locations` workload, one
-- `check --locations` child per iteration. --agreement also checks that
-- inline copies of a node absent from the file agree across ways.
-- Dataset, snapshot and variant bind the input for reservation, hashing and
-- pairing. No cache preparation: the typed runner does none either.
-- Child wall matches that workload; iteration wall includes Lua bookkeeping.
--
-- The fixed variant default is altw-keep-untagged. To reproduce the typed
-- runner's agreement-dependent default, pass --agreement yes --variant altw.
-- Bench params cannot express a default conditional on another param.
-- Exit 0 is clean, exit 1 violations, narrowed by expect.check-locations or
-- expect.check-locations-agreement, selected independently by agreement.
-- Before 854f9465 only 0 is trusted; since() uses commit ancestry, as in
-- index-integrity. Builds before the locations selector fail on its flag.
-- Marker stops record partial runs without claiming a full verdict.
-- The marked child supports --hotpath and --alloc, including marker stops.
-- Direct I/O, io_uring and compression are absent because the typed command
-- refuses them. Features, commit selection and iteration counts belong to
-- the bench runner.
--
--   brokkr bench --dry-run location-integrity --agreement yes,no
--   brokkr bench --alloc location-integrity --agreement yes --variant altw

local function expected(params)
    local declared = params.variant.expect
    if declared then
        if params.agreement == "yes" then return declared["check-locations-agreement"] end
        return declared["check-locations"]
    end
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
    local options = profiling.options("check-locations", params.stop)
    local r = binary_with(options, "check", "--locations", params.variant.file, {
        agreement = params.agreement == "yes",
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
    if r.exit ~= 0 and (r.exit ~= 1 or not since("854f9465")) then
        fail("check --locations ended with exit " .. tostring(r.exit)
            .. ", signal " .. tostring(r.signal) .. ": " .. r.stderr)
    end
    local verdict = "clean"
    if r.exit == 1 then verdict = "violations" end
    local want = expected(params)
    if want and verdict ~= want then
        fail("check --locations answered " .. verdict .. ", input location-check expectation declares " .. want)
    end
    record.check_exit = r.exit
    return verdict
end
