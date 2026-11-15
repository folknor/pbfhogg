-- ref-integrity: the typed `brokkr check-refs` workload, one `check --refs`
-- child per iteration, with optional relation-member checks and O_DIRECT.
-- Bind dataset, snapshot and variant for input reservation, hashing and
-- pairing. No cache preparation: the typed runner does none either.
-- The child wall matches that workload; iteration wall includes Lua work.
--
-- Exit 0 (clean) and exit 1 (violations) both complete the iteration
-- (benches/lib/answer.lua); the answer is not judged.
-- A marker stop returns the "stopped" verdict for a partial run.
-- The marked child supports --hotpath and --alloc, including marker stops.
-- Build features belong in the registry. Direct-I/O support is built for
-- both read modes.
--
--   brokkr bench --dry-run ref-integrity --check-relations yes,no
--   brokkr bench --hotpath ref-integrity --direct-io yes

local answer = require("answer")
local stops = require("stop")
local profiling = require("profiling")

function validate(params)
    stops.resolve(params.stop)
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
    answer.completed("check --refs", r)
    record.summary = r.stderr
end
