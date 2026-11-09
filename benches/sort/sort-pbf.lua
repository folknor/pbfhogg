-- sort-pbf: the typed sort workload, without cache preparation.
-- Dataset, snapshot and variant select the input; --force-sort yes forwards
-- sort's --force for inputs without indexdata. Compression is omitted by
-- default so the built revision chooses its own default. Both direct I/O
-- and io_uring output flags are forwarded, independently or together.
-- Outputs live under the host scratch's bench-iterations, one per iteration.
-- Profiling and marker stops combine; stopped output is not a complete PBF.
-- Build overrides, commits and iteration counts belong to the bench runner.
-- For controlled cold-cache comparisons, use sort-cold instead.
--
--   brokkr bench --dry-run sort-pbf --variant raw --force-sort yes
--   brokkr bench --alloc sort-pbf --io-uring yes --compression zstd:1

local compression = require("compression")
local stops = require("stop")
local profiling = require("profiling")

function validate(params)
    compression.parse(params.compression)
    stops.resolve(params.stop)
end

function bench(params)
    local options = profiling.options("sort", params.stop)
    local out = output("sorted.pbf")
    local r = binary_with(options, "sort", params.variant.file, {
        output = out,
        overwrite = true,
        force = params["force-sort"] == "yes",
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
