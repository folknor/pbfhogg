-- reencode-pbf: the typed repack workload, without cache preparation.
-- Dataset, snapshot and variant select the input. --elements-per-blob sets
-- the cap; omitted cap and compression keep the built revision's defaults.
-- --force-repack yes forwards --force for input without indexdata.
-- Both direct I/O and io_uring output flags are forwarded.
-- Outputs live under the host scratch's bench-iterations, one per iteration.
-- Profiling and marker stops combine; stopped output is not a complete PBF.
-- Build overrides, commits and iteration counts belong to the bench runner.
-- Snapshot promotion (--as-snapshot and --replace-snapshot) is omitted:
-- it needs the output-promotion API tracked by brokkr 145.
--
--   brokkr bench --dry-run reencode-pbf --elements-per-blob 1000 --force-repack yes
--   brokkr bench --alloc reencode-pbf --stop REPACK_WAYS --compression zstd:1

local compression = require("compression")
local stops = require("stop")
local profiling = require("profiling")

-- --elements-per-blob is a positive u32 by its typed domain in the
-- registry: the bench's own cap, narrower than the CLI's NonZeroUsize.
function validate(params)
    compression.parse(params.compression)
    stops.resolve(params.stop)
end

function bench(params)
    local options = profiling.options("repack", params.stop)
    local out = output("repack.pbf")
    local r = binary_with(options, "repack", params.variant.file, {
        output = out,
        overwrite = true,
        force = params["force-repack"] == "yes",
        ["elements-per-blob"] = params["elements-per-blob"],
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
