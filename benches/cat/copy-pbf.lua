-- copy-pbf: the typed cat workload, with no cache preparation.
-- Default: one-input indexdata passthrough. --type selects ways or relations;
-- --clean yes passes --clean version, also combined with --type. Compression
-- is forwarded as typed cat does, including on raw passthrough paths.
-- --dedupe yes repeats the same input, exactly the typed self-merge, without
-- the cache preparation and fincore requirements of dedupe-self/dedupe-pair.
-- Those benches remain the controlled-cache comparison workloads.
-- Outputs live under the host scratch's bench-iterations, one per iteration.
-- Profiling and marker stops combine; stopped output is not a complete PBF.
-- Build overrides, commits and iteration counts belong to the bench runner.
-- Snapshot promotion is explicitly refused: `brokkr man bench`,
-- "Where an iteration's files live", removes outputs at iteration end.
-- The Files and script builtins sections expose no
-- persistent output or dataset-registration operation. Keep typed cat for
-- --as-snapshot/--replace-snapshot until that API exists.
--
--   brokkr bench --dry-run copy-pbf --type way,relation --clean yes,no
--   brokkr bench --alloc copy-pbf --dedupe yes --io-uring yes

local compression = require("compression")
local stops = require("stop")
local profiling = require("profiling")

function validate(params)
    if params["as-snapshot"] or params["replace-snapshot"] == "yes" then
        fail("snapshot promotion is unavailable: bench outputs are iteration-owned and removed at iteration end")
    end
    if params.dedupe == "yes" and (params.type ~= "all" or params.clean == "yes") then
        fail("--dedupe yes takes neither --type nor --clean yes")
    end
    if params["io-uring"] == "yes" and params.dedupe ~= "yes" then
        fail("--io-uring yes requires --dedupe yes")
    end
    compression.parse(params.compression)
    stops.resolve(params.stop)
end

function bench(params)
    local options = profiling.options("cat", params.stop)
    local args = { "cat", params.variant.file }
    if params.dedupe == "yes" then args[#args + 1] = params.variant.file end
    local out = output("cat.pbf")
    args[#args + 1] = {
        output = out,
        overwrite = true,
        type = params.type ~= "all" and params.type or nil,
        clean = params.clean == "yes" and "version" or nil,
        dedupe = params.dedupe == "yes",
        ["direct-io"] = params["direct-io"] == "yes",
        ["io-uring"] = params["io-uring"] == "yes",
        compression = compression.parse(params.compression),
    }
    local r = binary_with(options, table.unpack(args)):ok()
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
