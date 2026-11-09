-- copy-pbf: the typed cat workload, with no cache preparation.
-- Default: one-input indexdata passthrough. --type selects ways or relations;
-- --clean yes passes --clean version, also combined with --type. Compression
-- is forwarded as typed cat does, including on raw passthrough paths.
-- --dedupe yes repeats the same input, exactly the typed self-merge, without
-- the cache preparation and fincore requirements of dedupe-self/dedupe-pair.
-- Those benches remain the controlled-cache comparison workloads.
-- Outputs live under the host scratch's bench-iterations, one per iteration.
-- Profiling and marker stops combine; stopped output is not a complete PBF.
-- Build overrides, commits and iteration counts belong to the bench runner.
-- Snapshot promotion is explicitly refused: `brokkr man bench`,
-- "Where an iteration's files live", removes outputs at iteration end.
-- The Files and script builtins sections expose no
-- persistent output or dataset-registration operation. Keep typed cat for
-- --as-snapshot/--replace-snapshot until that API exists.
--
--   brokkr bench --dry-run copy-pbf --type way,relation --clean yes,no
--   brokkr bench --alloc copy-pbf --dedupe yes --io-uring yes

local function stop_marker(value)
    local marker = value:gsub("^-", "")
    if marker:sub(-4) ~= "_END" then marker = marker .. "_END" end
    return marker
end

function validate(params)
    if params["as-snapshot"] or params["replace-snapshot"] == "yes" then
        fail("snapshot promotion is unavailable: bench outputs are iteration-owned and removed at iteration end")
    end
    if params.dedupe == "yes" and (params.type ~= "all" or params.clean == "yes") then
        fail("--dedupe yes takes neither --type nor --clean yes")
    end
    if params["io-uring"] == "yes" and params.dedupe ~= "yes" then
        fail("--io-uring yes requires --dedupe yes")
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
    local options = { hotpath = "cat" }
    if params.stop then options.stop_at = stop_marker(params.stop) end
    local args = { "cat", params.variant.file }
    if params.dedupe == "yes" then args[#args + 1] = params.variant.file end
    local out = output("cat.pbf")
    args[#args + 1] = {
        output = out,
        overwrite = true,
        type = params.type ~= "all" and params.type or nil,
        clean = params.clean == "yes" and "version" or nil,
        dedupe = params.dedupe == "yes",
        ["direct-io"] = params["direct-io"] == "yes",
        ["io-uring"] = params["io-uring"] == "yes",
        compression = params.compression and params.compression:match("^%s*(.-)%s*$"),
    }
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
