-- geocode-build: the typed build-geocode-index workload, one child per
-- iteration, without cache preparation. Always passes --force, matching the
-- typed runner's overwrite and allow-unindexed behavior on every revision.
-- The output directory is fresh; scratch and output live under the host's
-- scratch at bench-iterations and are removed when the iteration ends.
-- direct I/O is forwarded; io_uring and compression are refused because
-- this command writes an index, not a PBF. Profiling and marker stops combine.
-- Build overrides, commits and iteration counts belong to the bench runner.
--
--   brokkr bench --dry-run geocode-build --variant raw --direct-io yes
--   brokkr bench --alloc geocode-build --stop -GEOCODE_PASS2

local stops = require("stop")
local profiling = require("profiling")

function validate(params)
    if params["io-uring"] == "yes" then fail("build-geocode-index has no io_uring writer") end
    if params.compression then fail("build-geocode-index writes no PBF: compression is unsupported") end
    stops.resolve(params.stop)
end

function bench(params)
    local options = profiling.options("build-geocode-index", params.stop)
    local r = binary_with(options, "build-geocode-index", params.variant.file, {
        ["output-dir"] = output("geocode-index"),
        ["tmp-dir"] = scratch("geocode"),
        force = true,
        ["direct-io"] = params["direct-io"] == "yes",
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
