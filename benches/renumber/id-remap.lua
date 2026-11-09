-- id-remap: the typed `brokkr renumber` workload, one child per iteration.
-- Use the binary's default start IDs and external renumber path, as the
-- typed runner does; it exposes no start-ID or scratch-directory option.
-- No cache preparation. Child wall matches the typed workload; iteration
-- wall also includes Lua bookkeeping. Output is iteration-owned on the
-- project disk, rather than host scratch. Compression is omitted by
-- default so the built revision chooses its own default. Current renumber
-- also maps explicit zlib and zlib:6 to zlib:1; forward them unchanged.
-- Profiling and marker stops combine; a stop makes no complete-run claim.
-- io_uring is absent because the typed command refuses it. Build features,
-- commit selection and iteration counts belong to the bench runner.
--
--   brokkr bench --dry-run id-remap --direct-io yes
--   brokkr bench --alloc id-remap --compression zstd:1

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
    local options = { hotpath = "renumber" }
    if params.stop then options.stop_at = stop_marker(params.stop) end
    local r = binary_with(options, "renumber", params.variant.file, {
        output = output("renumbered.pbf"),
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
