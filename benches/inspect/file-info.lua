-- file-info: the typed `brokkr inspect` workload, one child per iteration.
-- Metadata, extended metadata, nodes and tags share the typed defaults;
-- tags always uses --min-count 999999999 to avoid sorting/printing rows.
-- No cache preparation. Child wall matches the typed workload; iteration
-- wall also includes Lua bookkeeping. Profiling and marker stops combine.
--
-- --expr takes one whole expression per flag, commas included, in order.
-- Features live in the registry; unsupported writer flags are refused.
--
--   brokkr bench --dry-run file-info --mode tags --expr 'a=b,c'
--   brokkr bench --alloc file-info --mode tags --expr 'a=b,c'

local stops = require("stop")
local profiling = require("profiling")

function validate(params)
    if params.mode ~= "tags" and (params.type or params.expr) then
        fail("--type and expressions require --mode tags")
    end
    -- --jobs is a u64 by its typed domain in the registry.
    if params.jobs and params.mode ~= "nodes" and params.mode ~= "tags" then
        fail("--jobs requires --mode nodes or tags")
    end
    for _, expr in ipairs(params.expr or {}) do
        if type(expr) ~= "string" or expr:sub(1, 1) == "-" or expr:find("%z") then
            fail("expressions must be strings without NUL and must not start with '-'")
        end
    end
    stops.resolve(params.stop)
end

function bench(params)
    local args = { "inspect" }
    if params.mode == "tags" then table.insert(args, "tags") end
    table.insert(args, params.variant.file)
    if params.mode == "tags" then
        for _, expr in ipairs(params.expr or {}) do table.insert(args, expr) end
        table.insert(args, { type = params.type, ["min-count"] = "999999999" })
    end
    table.insert(args, {
        nodes = params.mode == "nodes",
        extended = params.mode == "extended",
        jobs = params.jobs,
        ["direct-io"] = params["direct-io"] == "yes",
    })
    local options = profiling.options("inspect", params.stop)
    local r = binary_with(options, table.unpack(args)):ok()
    record.summary = r.stderr
    if r.stopped_at then
        record.stopped_at = r.stopped_at
        return "stopped"
    end
end
