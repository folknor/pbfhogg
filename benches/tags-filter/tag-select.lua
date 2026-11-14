-- tag-select: the typed `brokkr tags-filter` workload, one child per iteration.
-- PBF defaults to the two-pass reference-preserving path; OSC emits direct
-- matches into .osc.gz, matching the typed runner. No cache preparation.
-- Child wall matches the typed workload; iteration wall includes Lua work.
-- Output is iteration-owned on host scratch under bench-iterations. Compression is omitted
-- by default so the built revision chooses it. Profiling and stops combine.
--
-- --input selects the typed --variant for PBF, or --osc-seq for OSC. It is
-- bound under input-kind so only the actual file is reserved and hashed.
-- PBF defaults to indexed; OSC requires an explicit --input sequence.
-- The shared selector avoids binding an unused PBF or OSC input: defaults
-- are resolved before validate(), without conditional parameter binding.
-- Snapshots use their own OSC table, just as the typed command does.
-- --filter takes one whole readable expression, commas included. The built
-- revision remains the grammar authority. Features, commit selection and
-- counts belong to the runner.
--
--   brokkr bench --dry-run tag-select --filter 'w/highway=primary,secondary'
--   brokkr bench --alloc tag-select --input-kind osc --input 4705 --jobs 2

local compression = require("compression")
local stops = require("stop")
local profiling = require("profiling")

function validate(params)
    local expr = params.filter
    if type(expr) ~= "string" or expr:sub(1, 1) == "-" or expr:find("%z") then
        fail("filter must be a string without NUL and must not start with '-'")
    end
    if params["input-kind"] == "osc" then
        if params["omit-referenced"] == "yes" or params["invert-match"] == "yes"
            or params["remove-tags"] == "yes" or params["direct-io"] == "yes"
            or params.compression then
            fail("OSC input refuses reference flags, inversion, direct I/O and compression")
        end
    end
    if params["omit-referenced"] == "yes" and (params["remove-tags"] == "yes" or params.jobs) then
        fail("--omit-referenced refuses --remove-tags and --jobs")
    end
    -- --jobs is a u64 by its typed domain in the registry.
    compression.parse(params.compression)
    stops.resolve(params.stop)
end

function bench(params)
    local options = profiling.options("tags-filter", params.stop)
    local osc = params["input-kind"] == "osc"
    local r = binary_with(options, "tags-filter", params.input.file, params.filter, {
        ["input-kind"] = osc and "osc" or nil,
        output = output(osc and "filtered.osc.gz" or "filtered.pbf"),
        ["omit-referenced"] = params["omit-referenced"] == "yes",
        ["invert-match"] = params["invert-match"] == "yes",
        ["remove-tags"] = params["remove-tags"] == "yes",
        ["direct-io"] = params["direct-io"] == "yes",
        jobs = params.jobs,
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
