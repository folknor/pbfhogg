-- embed-locations: typed add-locations-to-ways without cache preparation.
-- Omitted index type and compression keep the built revision's defaults.
-- sparse, external and auto share the same flags; pbfhogg judges unsupported
-- combinations and input eligibility, just as it does for the typed runner.
-- inject-prepass emits the opt-in, osmium-incompatible wire extensions.
-- ignore-missing-nodes additionally exposes pbfhogg's missing-reference policy;
-- the typed runner does not currently expose it. Omit it for older commits.
-- Output and declared --tmp-dir scratch live under the host scratch's
-- bench-iterations, isolated per iteration, including planet external joins.
-- Profiling and marker stops combine; stopped output is not a complete PBF.
-- FOO and -FOO resolve to FOO_END; the API cannot express the typed runner's
-- preference for a verbatim bare FOO marker before its FOO_END fallback.
-- Build overrides, commits and iteration counts belong to the bench runner.
--
--   brokkr bench --dry-run embed-locations --index-type sparse,external,auto
--   brokkr bench --alloc embed-locations --index-type external --stop -ALTW_EXT

local function stop_marker(value)
    if value:sub(1, 1) == "-" then return value:sub(2) .. "_END" end
    local marker = value
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
    local options = { hotpath = "altw" }
    if params.stop then options.stop_at = stop_marker(params.stop) end
    local out = output("locations.osm.pbf")
    local r = binary_with(options, "add-locations-to-ways", params.variant.file, {
        output = out,
        overwrite = true,
        force = params["force-altw"] == "yes",
        ["index-type"] = params["index-type"],
        ["inject-prepass"] = params["inject-prepass"] == "yes",
        ["keep-untagged-nodes"] = params["keep-untagged-nodes"] == "yes",
        ["ignore-missing-nodes"] = params["ignore-missing-nodes"] == "yes",
        ["tmp-dir"] = scratch("altw"),
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
