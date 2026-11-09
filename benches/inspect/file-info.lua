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

local function stop_marker(value)
    local marker = value:gsub("^-", "")
    if marker:sub(-4) ~= "_END" then marker = marker .. "_END" end
    return marker
end

function validate(params)
    if params.mode ~= "tags" and (params.type or params.expr) then
        fail("--type and expressions require --mode tags")
    end
    if params.jobs then
        if params.mode ~= "nodes" and params.mode ~= "tags" then
            fail("--jobs requires --mode nodes or tags")
        end
        local digits = params.jobs:gsub("^%+", "")
        local canonical = digits:gsub("^0+", "")
        if canonical == "" then canonical = "0" end
        if not digits:match("^%d+$") or #canonical > 20
            or (#canonical == 20 and canonical > "18446744073709551615") then
            fail("--jobs must be an unsigned 64-bit integer")
        end
    end
    for _, expr in ipairs(params.expr or {}) do
        if type(expr) ~= "string" or expr:sub(1, 1) == "-" or expr:find("%z") then
            fail("expressions must be strings without NUL and must not start with '-'")
        end
    end
    if params.stop and (not params.stop:match("^%-?[%w_]+$")
        or stop_marker(params.stop) == "_END") then
        fail("--stop must be a marker name: FOO_END, FOO or -FOO")
    end
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
    local options = { hotpath = "inspect" }
    if params.stop then options.stop_at = stop_marker(params.stop) end
    local r = binary_with(options, table.unpack(args)):ok()
    record.summary = r.stderr
    if r.stopped_at then
        record.stopped_at = r.stopped_at
        return "stopped"
    end
end
