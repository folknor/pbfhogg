-- apply-diff: typed apply-changes without cache preparation.
-- --osc selects one sequence from the selected snapshot's own OSC table.
-- Omitted --osc binds the sole entry via default_only; zero or multiple
-- entries refuse before hashing or building. No primary-table fallback.
-- --locations-on-ways yes passes the flag; choose --variant
-- altw-keep-untagged explicitly so changed ways can resolve base nodes.
-- Omitted jobs and compression keep the built revision's defaults.
-- Outputs live under host scratch's bench-iterations, one per iteration.
-- Profiling and marker stops combine; stopped output is not a complete PBF.
-- Build overrides, commits and iteration counts belong to the bench runner.
--
--   brokkr bench --dry-run apply-diff --osc 4705 --jobs 0,2
--   brokkr bench --alloc apply-diff --io-uring yes --compression zstd:1

local compression = require("compression")
local stops = require("stop")
local profiling = require("profiling")

-- --jobs is a u64 by its typed domain in the registry.
function validate(params)
    compression.parse(params.compression)
    stops.resolve(params.stop)
end

function bench(params)
    local options = profiling.options("apply", params.stop)
    local out = output("applied.osm.pbf")
    local r = binary_with(options, "apply-changes", params.variant.file, params.osc.file, {
        output = out,
        overwrite = true,
        ["locations-on-ways"] = params["locations-on-ways"] == "yes",
        jobs = params.jobs,
        ["direct-io"] = params["direct-io"] == "yes",
        ["io-uring"] = params["io-uring"] == "yes",
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
