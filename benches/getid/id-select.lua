-- id-select: the typed `brokkr getid` workload, one child per iteration.
-- Use dataset-level getid_ids even for snapshots, as the typed runner does;
-- otherwise use its fixed Denmark IDs. Keep their order and canonicalize
-- leading zeros without converting 64-bit IDs to Lua numbers.
-- No cache preparation. Child wall matches the typed workload; iteration
-- wall also includes Lua bookkeeping. Output is iteration-owned under
-- bench-iterations on the host's resolved scratch.
-- Profiling and marker stops combine; a stop makes no complete-run claim.
-- io_uring is absent because typed getid refuses it too. Build features,
-- commit selection and iteration counts belong to the bench runner.
--
--   brokkr bench --dry-run id-select --add-referenced yes
--   brokkr bench --alloc id-select --invert yes --compression zstd:1

local DEFAULT_IDS = {
    "n115722", "n115723", "n115724", "w2080", "w2081", "w2082",
    "r174", "r213", "r339",
}

local function selected_ids(params)
    local ids = {}
    for _, id in ipairs(params.dataset.getid_ids or DEFAULT_IDS) do
        local digits = id:sub(2):gsub("^0+", "")
        if digits == "" then digits = "0" end
        table.insert(ids, id:sub(1, 1) .. digits)
    end
    return ids
end

local compression = require("compression")
local stops = require("stop")
local profiling = require("profiling")

function validate(params)
    if params["add-referenced"] == "yes" and params.invert == "yes" then
        fail("--add-referenced and --invert are mutually exclusive")
    end
    compression.parse(params.compression)
    stops.resolve(params.stop)
end

function bench(params)
    local ids = selected_ids(params)
    local args = { "getid", params.variant.file }
    for _, id in ipairs(ids) do table.insert(args, id) end
    table.insert(args, {
        output = output("selected.pbf"),
        ["add-referenced"] = params["add-referenced"] == "yes",
        invert = params.invert == "yes",
        ["direct-io"] = params["direct-io"] == "yes",
        compression = compression.parse(params.compression),
    })
    local options = profiling.options("getid", params.stop)
    local r = binary_with(options, table.unpack(args)):ok()
    record.ids = ids
    record.summary = r.stderr
    if params.verbose == "yes" then
        print(r.stdout)
        print(r.stderr)
    end
    if r.stopped_at then
        record.stopped_at = r.stopped_at
        return "stopped"
    end
end
