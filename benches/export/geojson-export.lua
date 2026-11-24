-- geojson-export: one export child per iteration, writing its GeoJSON to an
-- iteration-owned file under bench-iterations on the host's resolved
-- scratch, so the output's size, not a captured stdout, bounds the run.
-- Dataset, snapshot and variant bind the input; ways need a LocationsOnWays
-- variant (the default), which export refuses otherwise. No cache
-- preparation. Child wall is the measurement; the iteration wall also
-- includes Lua bookkeeping. Export emits no markers, so there is no stop.
-- io_uring, direct I/O and compression are absent because export has none.
--
--   brokkr bench --dry-run geojson-export --dataset planet --variant altw
--   brokkr bench geojson-export --type node --metadata yes

function validate(params)
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
    local r
    if params.expression ~= nil then
        r = binary_with({ hotpath = "export", measured = true }, "export",
            params.variant.file, flags, params.expression):ok()
    else
        r = binary_with({ hotpath = "export", measured = true }, "export",
            params.variant.file, flags):ok()
    end
    record.summary = r.stderr
    if params.verbose == "yes" then
        print(r.stdout)
        print(r.stderr)
    end
end
