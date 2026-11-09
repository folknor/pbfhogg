-- sort-pbf: the typed sort workload, without cache preparation.
-- Dataset, snapshot and variant select the input; --force-sort yes forwards
-- sort's --force for inputs without indexdata. Compression is omitted by
-- default so the built revision chooses its own default. Both direct I/O
-- and io_uring output flags are forwarded, independently or together.
-- Outputs live under the host scratch's bench-iterations, one per iteration.
-- Profiling and marker stops combine; stopped output is not a complete PBF.
-- Build overrides, commits and iteration counts belong to the bench runner.
-- For controlled cold-cache comparisons, use sort-cold instead.
--
--   brokkr bench --dry-run sort-pbf --variant raw --force-sort yes
--   brokkr bench --alloc sort-pbf --io-uring yes --compression zstd:1

local function stop_marker(value)
    local marker = value:gsub("^-", "")
    if marker:sub(-4) ~= "_END" then marker = marker .. "_END" end
    return marker
end

function validate(params)
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
    local options = { hotpath = "sort" }
    if params.stop then options.stop_at = stop_marker(params.stop) end
    local out = output("sorted.pbf")
    local r = binary_with(options, "sort", params.variant.file, {
        output = out,
        overwrite = true,
        force = params["force-sort"] == "yes",
        ["io-uring"] = params["io-uring"] == "yes",
        ["direct-io"] = params["direct-io"] == "yes",
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
    record.output_bytes = filesize(out)
end
