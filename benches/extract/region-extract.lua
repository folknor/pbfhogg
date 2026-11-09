-- region-extract: typed extract and multi-extract without cache preparation.
-- Single mode uses a bbox (explicit or dataset-level) or one dataset polygon.
-- Multi mode cuts the bbox into equal longitude strips, then appends the
-- selected polygons in order. Polygon params bind files, not unchecked paths.
-- Omitted strategy means all for single, simple for multi, matching the typed
-- defaults. all runs simple, complete and smart sequentially; compare their
-- child walls individually, since the iteration wall includes all three and
-- config generation. Each strategy has fresh outputs under the host scratch's
-- bench-iterations. Hidden config inputs/outputs are declared on the child.
-- Profiling and marker stops combine; a stop ends the iteration immediately.
-- Build overrides, commits and iteration counts belong to the bench runner.
--
--   brokkr bench --dry-run region-extract --bbox '-2,50,0,52' --strategy smart
--   brokkr bench --alloc region-extract --mode multi --regions 5 --strategy all
--   brokkr bench region-extract --mode multi --polygon city --polygon coast

local compression = require("compression")
local stops = require("stop")
local profiling = require("profiling")

local function bbox(params)
    local value = params.bbox or params.dataset.bbox
    if type(value) ~= "string" then fail("supply --bbox or configure the dataset bbox") end
    local coords = {}
    for part in (value .. ","):gmatch("(.-),") do
        local n = tonumber(part:match("^%s*(.-)%s*$"))
        if not n or n ~= n or n == math.huge or n == -math.huge then
            fail("bbox must contain four finite comma-separated coordinates")
        end
        table.insert(coords, n)
    end
    if #coords ~= 4 then fail("bbox must contain four finite comma-separated coordinates") end
    return coords, value
end

function validate(params)
    local polygons = params.polygon or {}
    if params.mode == "single" then
        if params.regions then fail("--regions requires --mode multi") end
        if #polygons > 1 then fail("single mode takes at most one polygon") end
        if #polygons > 0 and params.bbox then fail("choose --bbox or --polygon in single mode") end
    end
    if params.mode == "multi" or #polygons == 0 then
        local coords = bbox(params)
        if params.mode == "multi" then
            local digits = (params.regions or "5"):gsub("^%+", "")
            local canonical = digits:gsub("^0+", "")
            if not digits:match("^%d+$") or canonical == "" or #canonical > 20
                or (#canonical == 20 and canonical > "18446744073709551615") then
                fail("--regions must be a positive unsigned 64-bit integer")
            end
            local width = (coords[3] - coords[1]) / tonumber(digits)
            if width ~= width or width == math.huge or width == -math.huge then
                fail("bbox geometry produced a non-finite coordinate")
            end
        end
    end
    compression.parse(params.compression)
    stops.resolve(params.stop)
end

function bench(params)
    local strategy = params.strategy or (params.mode == "multi" and "simple" or "all")
    local strategies = strategy == "all" and { "simple", "complete", "smart" } or { strategy }
    record.strategies = {}
    for _, current in ipairs(strategies) do
        local options = profiling.options(current, params.stop)
        local args = { "extract", params.variant.file }
        local outputs = {}
        if params.mode == "multi" then
            local coords = bbox(params)
            local count = tonumber(params.regions or "5")
            local width = (coords[3] - coords[1]) / count
            local extracts = {}
            for i = 0, count - 1 do
                local out = output(current .. "-strip-" .. i .. ".osm.pbf")
                table.insert(outputs, out)
                table.insert(extracts, {
                    output = out.path,
                    bbox = { coords[1] + width * i, coords[2],
                        i + 1 == count and coords[3] or coords[1] + width * (i + 1), coords[4] },
                })
            end
            options.reads = {}
            for i, polygon in ipairs(params.polygon or {}) do
                local out = output(current .. "-polygon-" .. i .. ".osm.pbf")
                table.insert(outputs, out)
                table.insert(options.reads, polygon.file)
                table.insert(extracts, { output = out.path, polygon_file = polygon.file.path })
            end
            local cfg = output(current .. "-config.json")
            write(cfg, json.encode({ extracts = extracts }))
            table.insert(options.reads, cfg)
            options.writes = outputs
            table.insert(args, { config = cfg })
        else
            local out = output(current .. ".osm.pbf")
            table.insert(outputs, out)
            table.insert(args, { output = out })
            if params.polygon and #params.polygon > 0 then
                table.insert(args, { polygon = params.polygon[1].file })
            else
                local _, value = bbox(params)
                table.insert(args, "-b=" .. value)
            end
        end
        table.insert(args, {
            simple = current == "simple",
            smart = current == "smart",
            overwrite = true,
            ["direct-io"] = params["direct-io"] == "yes",
            ["io-uring"] = params["io-uring"] == "yes",
            compression = compression.parse(params.compression),
        })
        local r = binary_with(options, table.unpack(args)):ok()
        local result = { strategy = current, summary = r.stderr }
        table.insert(record.strategies, result)
        if params.verbose == "yes" then
            print(r.stdout)
            print(r.stderr)
        end
        if r.stopped_at then
            result.stopped_at = r.stopped_at
            return "stopped"
        end
        result.output_bytes = {}
        for _, out in ipairs(outputs) do table.insert(result.output_bytes, filesize(out)) end
    end
end
