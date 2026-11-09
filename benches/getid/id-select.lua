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

local function stop_marker(value)
    local marker = value:gsub("^-", "")
    if marker:sub(-4) ~= "_END" then marker = marker .. "_END" end
    return marker
end

function validate(params)
    if params["add-referenced"] == "yes" and params.invert == "yes" then
        fail("--add-referenced and --invert are mutually exclusive")
    end
    if params.compression then
        local value = params.compression:match("^%s*(.-)%s*$")
        local level = value:match("^zstd:([%+%-]?%d+)$")
        local zlib = value:match("^zlib:(%+?%d+)$")
        local valid_zlib = zlib and tonumber(zlib) <= 9
        local valid_zstd = false
        if level then
            local n = tonumber(level)
            valid_zstd = n ~= nil and n >= -2147483648 and n <= 2147483647
        end
        if value ~= "none" and value ~= "zlib" and value ~= "zstd"
            and not valid_zlib and not valid_zstd then
            fail("--compression must be none, zlib[:0-9] or zstd[:signed 32-bit level]")
        end
    end
    if params.stop then
        if not params.stop:match("^%-?[%w_]+$") or stop_marker(params.stop) == "_END" then
            fail("--stop must be a marker name: FOO_END, FOO or -FOO")
        end
    end
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
        compression = params.compression and params.compression:match("^%s*(.-)%s*$"),
    })
    local options = { hotpath = "getid" }
    if params.stop then options.stop_at = stop_marker(params.stop) end
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
