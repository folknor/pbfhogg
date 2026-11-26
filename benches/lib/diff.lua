-- Shared general diff workload. Child wall, rather than iteration wall,
-- excludes merge preparation. No forced page-cache state.
local answer = require("answer")
local profiling = require("profiling")
local stops = require("stop")
local M = {}

function M.validate(params)
    stops.resolve(params.stop)
end

-- Scan all data blobs: inspect --indexed only probes the first one.
function M.unindexed(input)
    local r = binary("inspect", input, { get = "indexed" }):ok()
    local indexed = r.stdout:match("^%s*(%a+)%s*$")
    if indexed ~= "true" and indexed ~= "false" then
        fail("inspect --get indexed returned an unexpected answer: " .. r.stdout)
    end
    return indexed == "false"
end

-- Exit 0 (same) and exit 1 (differs) both complete the iteration; the
-- answer is not judged (benches/lib/answer.lua).
function M.run(params, old, new, output_name)
    local out = output(output_name or (params.format == "osc" and "diff.osc" or "diff.txt"))
    local flags = { output = out, overwrite = true, jobs = params.jobs, type = params.type,
        ["direct-io"] = params["direct-io"] == "yes" }
    if params.format == "osc" then
        flags.format = "osc"
        flags["tmp-dir"] = scratch("diff")
    else
        flags["suppress-common"] = true
    end
    local options = profiling.options("diff", params.stop)
    options.measured = true
    local r = binary_with(options, "diff", old, new, flags)
    record.summary = r.stderr
    if params.verbose == "yes" then print(r.stdout); print(r.stderr) end
    if r.stopped_at then
        record.stopped_at = r.stopped_at
        return "stopped"
    end
    answer.completed("diff", r)
    record.output_bytes = filesize(out)
end

return M
