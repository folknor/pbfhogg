-- tag-select: the typed `brokkr tags-filter` workload, one child per iteration.
-- PBF defaults to the two-pass reference-preserving path; OSC emits direct
-- matches into .osc.gz, matching the typed runner. No cache preparation.
-- Child wall matches the typed workload; iteration wall includes Lua work.
-- Output is iteration-owned on the project disk. Compression is omitted
-- by default so the built revision chooses it. Profiling and stops combine.
--
-- --input selects the typed --variant for PBF, or --osc-seq for OSC. It is
-- bound under input-kind so only the actual file is reserved and hashed.
-- OSC sequences are explicit, even when the host has only one registered.
-- Snapshots use their own OSC table, just as the typed command does.
-- --filter is one string; for commas use --expr-hex, a hex-encoded ASCII
-- JSON array holding exactly one string (JSON unicode escapes are allowed).
-- For example 5b22613d622c63225d encodes ["a=b,c"]. No expression parsing
-- is duplicated here: the built revision remains the grammar authority.
-- io_uring is refused, matching the typed runner. Features, commit selection
-- and iteration counts belong to the bench runner.
--
--   brokkr bench --dry-run tag-select --omit-referenced yes --invert-match yes
--   brokkr bench --alloc tag-select --input-kind osc --input 4705 --jobs 2

local expression_params = require("expressions")

local function filter_expression(params)
    if params["expr-hex"] then
        local values = expression_params.expressions(params)
        if #values ~= 1 then fail("--expr-hex must contain exactly one filter string") end
        return values[1]
    end
    return params.filter or "w/highway=primary"
end

local function stop_marker(value)
    local marker = value:gsub("^-", "")
    if marker:sub(-4) ~= "_END" then marker = marker .. "_END" end
    return marker
end

function validate(params)
    if params.filter and params["expr-hex"] then fail("choose --filter or --expr-hex") end
    local expr = filter_expression(params)
    if type(expr) ~= "string" or expr:sub(1, 1) == "-" or expr:find("%z") then
        fail("filter must be a string without NUL and must not start with '-'")
    end
    if params["io-uring"] == "yes" then fail("tags-filter has no io_uring writer") end
    if params["input-kind"] == "osc" then
        if params["omit-referenced"] == "yes" or params["invert-match"] == "yes"
            or params["remove-tags"] == "yes" or params["direct-io"] == "yes"
            or params.compression then
            fail("OSC input refuses reference flags, inversion, direct I/O and compression")
        end
    end
    if params["omit-referenced"] == "yes" and (params["remove-tags"] == "yes" or params.jobs) then
        fail("--omit-referenced refuses --remove-tags and --jobs")
    end
    if params.jobs then
        local digits = params.jobs:gsub("^%+", "")
        local canonical = digits:gsub("^0+", "")
        if canonical == "" then canonical = "0" end
        if not digits:match("^%d+$") or #canonical > 20
            or (#canonical == 20 and canonical > "18446744073709551615") then
            fail("--jobs must be an unsigned 64-bit integer")
        end
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
    local options = { hotpath = "tags-filter" }
    if params.stop then options.stop_at = stop_marker(params.stop) end
    local osc = params["input-kind"] == "osc"
    local r = binary_with(options, "tags-filter", params.input.file, filter_expression(params), {
        ["input-kind"] = osc and "osc" or nil,
        output = output(osc and "filtered.osc.gz" or "filtered.pbf"),
        ["omit-referenced"] = params["omit-referenced"] == "yes",
        ["invert-match"] = params["invert-match"] == "yes",
        ["remove-tags"] = params["remove-tags"] == "yes",
        ["direct-io"] = params["direct-io"] == "yes",
        jobs = params.jobs,
        compression = params.compression and params.compression:match("^%s*(.-)%s*$"),
    }):ok()
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
