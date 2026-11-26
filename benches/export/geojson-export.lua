-- geojson-export: one export child per iteration, writing its GeoJSON to an
-- iteration-owned file under bench-iterations on the host's resolved
-- scratch, so the output's size, not a captured stdout, bounds the run.
-- Dataset, snapshot and variant bind the input; ways need a LocationsOnWays
-- variant (the default), which export refuses otherwise. No cache
-- preparation. Child wall is the measurement; the iteration wall also
-- includes Lua bookkeeping. Export emits no markers, so there is no stop.
-- --bbox restricts export to the requested box.
-- io_uring, direct I/O and compression are absent because export has none.
--
--   brokkr bench --dry-run geojson-export --dataset planet --variant altw
--   brokkr bench geojson-export --type node --metadata yes

local bboxes = require("bbox")

function validate(params)
    if params.bbox ~= nil then bboxes.parse(params.bbox) end
    if params.expression ~= nil and params.expression == "" then
        fail("--expression needs a tag expression")
    end
end

function bench(params)
    local ext = "geojsonseq"
    if params.format == "geojson" then ext = "geojson" end
    local flags = {
        output = output("out." .. ext),
        format = params.format,
        metadata = params.metadata == "yes",
    }
    if params.type ~= "all" then flags.type = params.type end
    local args = { "export", params.variant.file, flags }
    -- The equals form also accepts a bbox starting with a negative longitude.
    if params.bbox ~= nil then args[#args + 1] = "--bbox=" .. params.bbox end
    if params.expression ~= nil then args[#args + 1] = params.expression end
    local r = binary_with({ hotpath = "export", measured = true }, table.unpack(args)):ok()
    record.summary = r.stderr
    if params.verbose == "yes" then
        print(r.stdout)
        print(r.stderr)
    end
end
