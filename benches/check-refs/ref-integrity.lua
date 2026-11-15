-- ref-integrity: the typed `brokkr check-refs` workload, one `check --refs`
-- child per iteration, with optional relation-member checks.
-- Bind dataset, snapshot and variant for input reservation, hashing and
-- pairing. No cache preparation: the typed runner does none either.
-- The child wall matches that workload; iteration wall includes Lua work.
--
-- Exit 0 (clean) and exit 1 (violations) both complete the iteration
-- (benches/lib/answer.lua); the answer is not judged.
-- A marker stop returns the "stopped" verdict for a partial run.
-- The marked child supports --hotpath and --alloc, including marker stops.
-- Build features belong in the registry. There is no direct-io param: the
-- ref check reads through buffered parallel preads, so --direct-io never
-- reached it, and from 930ddfc9 `check --refs` alone refuses the flag.
--
--   brokkr bench --dry-run ref-integrity --check-relations yes,no
--   brokkr bench --hotpath ref-integrity

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
    })
    if r.stopped_at then
        record.stopped_at = r.stopped_at
        return "stopped"
    end
    answer.completed("check --refs", r)
    record.summary = r.stderr
end
