-- merge-diffs: typed merge-changes without cache preparation.
-- --osc takes a sequence, comma list, repeated flags, all or LO..HI ranges.
-- Every input is bound, reserved, hashed and paired in supplied order.
-- Snapshot selection uses its own OSC table, without primary fallback.
-- --simplify yes keeps the last change per object; no retains every change.
-- Output and declared spill scratch live under host scratch's
-- bench-iterations, isolated per iteration. The PBF-only typed --variant
-- has no role in this workload.
-- Profiling and marker stops combine; stopped output is not a complete OSC.
-- Build overrides, commits and iteration counts belong to the bench runner.
--
--   brokkr bench --dry-run merge-diffs --dataset germany --osc 4705..4707 --simplify yes,no
--   brokkr bench --hotpath merge-diffs --osc 4705

local stops = require("stop")
local profiling = require("profiling")

function validate(params)
    if #params.osc == 0 then fail("--osc must select at least one sequence") end
    stops.resolve(params.stop)
end

function bench(params)
    local options = profiling.options("merge", params.stop)
    local out = output("merged.osc.gz")
    local args = { "merge-changes" }
    for _, osc in ipairs(params.osc) do args[#args + 1] = osc.file end
    local flags = {
        output = out,
        overwrite = true,
        simplify = params.simplify == "yes",
    }
    if params.simplify ~= "yes" and #params.osc > 1 then
        flags["tmp-dir"] = scratch("merge")
    end
    args[#args + 1] = flags
    local r = binary_with(options, table.unpack(args)):ok()
    record.summary = r.stderr
    if params.verbose == "yes" then
        print(r.stdout)
        print(r.stderr)
    end
    if r.stopped_at then
        record.stopped_at = r.stopped_at
        return "stopped"
    end
    record.output_bytes = filesize(out)
end
