-- time-snapshot: the typed `brokkr time-filter` workload, one child per iteration.
-- Preserve its fixed 2024-01-01T00:00:00Z cutoff; typed time-filter exposes
-- no timestamp option. No cache preparation. Child wall matches the typed
-- workload; iteration wall also includes Lua bookkeeping. Output is
-- iteration-owned under bench-iterations on the host's resolved scratch.
-- Profiling and marker stops combine; a stop makes no complete-run claim.
-- io_uring is absent because the typed command refuses it. Build features,
-- commit selection and iteration counts belong to the bench runner.
--
--   brokkr bench --dry-run time-snapshot --direct-io yes
--   brokkr bench --alloc time-snapshot --compression zstd:1

local compression = require("compression")
local stops = require("stop")
local profiling = require("profiling")

function validate(params)
    compression.parse(params.compression)
    stops.resolve(params.stop)
end

function bench(params)
    local options = profiling.options("time-filter", params.stop)
    local r = binary_with(options, "time-filter", params.variant.file,
        "2024-01-01T00:00:00Z", {
            output = output("snapshot.pbf"),
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
end
