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
-- FOO, -FOO and -FOO_END resolve to FOO_END; FOO_END stays verbatim.
-- Build overrides, commits and iteration counts belong to the bench runner.
--
--   brokkr bench --dry-run embed-locations --index-type sparse,external,auto
--   brokkr bench --alloc embed-locations --index-type external --stop -ALTW_EXT

local compression = require("compression")
local stops = require("stop")
local profiling = require("profiling")

function validate(params)
    compression.parse(params.compression)
    stops.resolve(params.stop)
end

function bench(params)
    local options = profiling.options("altw", params.stop)
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
        compression = compression.parse(params.compression),
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
