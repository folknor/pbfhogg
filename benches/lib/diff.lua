-- Shared general diff workload. Child wall, rather than iteration wall,
-- excludes merge preparation. No forced page-cache state.
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

function M.run(params, old, new, want)
    local out = output(params.format == "osc" and "diff.osc" or "diff.txt")
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
    if r.exit ~= 0 and (r.exit ~= 1 or not since("854f9465")) then
        fail("diff ended with exit " .. tostring(r.exit) .. ", signal " .. tostring(r.signal) .. ": " .. r.stderr)
    end
    local verdict = r.exit == 0 and "same" or "differs"
    if want and want ~= verdict then fail("diff answered " .. verdict .. ", declared " .. want) end
    record.diff_exit = r.exit
    record.output_bytes = filesize(out)
    return verdict
end

return M
