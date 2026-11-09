-- id-remap: the typed `brokkr renumber` workload, one child per iteration.
-- Use the binary's default start IDs and external renumber path, as the
-- typed runner does; it exposes no start-ID or scratch-directory option.
-- No cache preparation. Child wall matches the typed workload; iteration
-- wall also includes Lua bookkeeping. Output is iteration-owned under
-- bench-iterations on the host's resolved scratch. Compression is omitted by
-- default so the built revision chooses its own default. Current renumber
-- also maps explicit zlib and zlib:6 to zlib:1; forward them unchanged.
-- Profiling and marker stops combine; a stop makes no complete-run claim.
-- io_uring is absent because the typed command refuses it. Build features,
-- commit selection and iteration counts belong to the bench runner.
--
--   brokkr bench --dry-run id-remap --direct-io yes
--   brokkr bench --alloc id-remap --compression zstd:1

local compression = require("compression")
local stops = require("stop")
local profiling = require("profiling")

function validate(params)
    compression.parse(params.compression)
    stops.resolve(params.stop)
end

function bench(params)
    local options = profiling.options("renumber", params.stop)
    local r = binary_with(options, "renumber", params.variant.file, {
        output = output("renumbered.pbf"),
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
