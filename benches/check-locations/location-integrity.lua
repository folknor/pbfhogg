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
-- Exit 0 (clean) and exit 1 (violations) both complete the iteration
-- (benches/lib/answer.lua); the answer is not judged. Builds before the
-- locations selector fail on its flag.
-- A marker stop returns the "stopped" verdict for a partial run.
-- The marked child supports --hotpath and --alloc, including marker stops.
-- Direct I/O, io_uring and compression are absent because the typed command
-- refuses them. Features, commit selection and iteration counts belong to
-- the bench runner.
--
--   brokkr bench --dry-run location-integrity --agreement yes,no
--   brokkr bench --alloc location-integrity --agreement yes --variant altw

local answer = require("answer")
local stops = require("stop")
local profiling = require("profiling")

function validate(params)
    stops.resolve(params.stop)
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
    answer.completed("check --locations", r)
end
