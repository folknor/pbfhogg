-- perturb-pbf: the typed degrade workload, without cache preparation.
-- Dataset, snapshot and variant select the input. --strip-indexdata,
-- --strip-locations, --unsort and --drop-ids compose; at least one must
-- be enabled. --force allows unindexed input on the decode path.
-- Omitted compression keeps the built revision's default. Indexdata-only
-- stripping uses the binary's metadata passthrough path.
-- Both direct I/O and io_uring output flags are forwarded.
-- Outputs live under the host scratch's bench-iterations, one per iteration.
-- Profiling and marker stops combine; stopped output is not a complete PBF.
-- Build overrides, commits and iteration counts belong to the bench runner.
-- Snapshot promotion (--as-snapshot and --replace-snapshot) is omitted:
-- it needs the output-promotion API tracked by brokkr 145.
--
--   brokkr bench --dry-run perturb-pbf --strip-indexdata yes --unsort yes
--   brokkr bench --hotpath perturb-pbf --strip-locations yes --stop DEGRADE_WAYS

local compression = require("compression")
local stops = require("stop")
local profiling = require("profiling")

function validate(params)
    if params["strip-indexdata"] ~= "yes" and params["strip-locations"] ~= "yes"
        and params.unsort ~= "yes" and params["drop-ids"] == nil then
        fail("at least one perturbation is required: --strip-indexdata yes, --strip-locations yes, --unsort yes or --drop-ids N:SEED")
    end
    compression.parse(params.compression)
    stops.resolve(params.stop)
end

function bench(params)
    local options = profiling.options("degrade", params.stop)
    local out = output("degrade.pbf")
    local r = binary_with(options, "degrade", params.variant.file, {
        output = out,
        overwrite = true,
        ["strip-indexdata"] = params["strip-indexdata"] == "yes",
        ["strip-locations"] = params["strip-locations"] == "yes",
        unsort = params.unsort == "yes",
        ["drop-ids"] = params["drop-ids"],
        force = params.force == "yes",
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
