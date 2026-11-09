-- time-snapshot: the typed `brokkr time-filter` workload, one child per iteration.
-- Preserve its fixed 2024-01-01T00:00:00Z cutoff; typed time-filter exposes
-- no timestamp option. No cache preparation. Child wall matches the typed
-- workload; iteration wall also includes Lua bookkeeping. Output is
-- iteration-owned on the project disk, rather than host scratch.
-- Profiling and marker stops combine; a stop makes no complete-run claim.
-- io_uring is absent because the typed command refuses it. Build features,
-- commit selection and iteration counts belong to the bench runner.
--
--   brokkr bench --dry-run time-snapshot --direct-io yes
--   brokkr bench --alloc time-snapshot --compression zstd:1

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
    local options = { hotpath = "time-filter" }
    if params.stop then options.stop_at = stop_marker(params.stop) end
    local r = binary_with(options, "time-filter", params.variant.file,
        "2024-01-01T00:00:00Z", {
            output = output("snapshot.pbf"),
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
end
