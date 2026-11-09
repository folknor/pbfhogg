-- merge-workload: typed merge's synthetic workload, without cache preparation.
-- Builds the pbfhogg-cli package's pbfhogg bin, not an example or harness.
-- Each selected matrix value is an arm with one child, except --uring yes,
-- which runs buffered then uring children on the same build for merge.
-- Uses the child's own library workload; no command-layer substitute.
-- Child elapsed_ms is retained separately: iteration wall includes launch,
-- setup and teardown, and is not the typed runner's self-reported wall.
-- Outputs and staging live under host scratch's bench-iterations directory.
-- Stops return a stopped verdict; their outputs are incomplete.
-- Build overrides, commits and iteration counts belong to the bench runner.
-- The default build enables linux-io-uring even for buffered-only arms.
-- Override bench features in config when matching a typed non-uring build.
-- OSC selection is primary-only, as in the typed merge runner.
--
--   brokkr bench --dry-run merge-workload
--   brokkr bench --alloc merge-workload

local workload = require("workload")
local profiling = require("profiling")
local compression = require("compression")

function validate(params)
    workload.validate(params)
    if #params["osc-seq"] ~= 1 then fail("--osc-seq must select exactly one sequence") end
end

function bench(params)
    local modes = params.uring == "yes" and { "buffered", "uring" } or { "buffered" }
    record.cells = {}
    local stopped = false
    for _, mode in ipairs(modes) do
        local out = output(mode .. ".osm.pbf")
        local r = binary_with(profiling.options(mode, params.stop),
            "bench-merge", params.variant.file, params["osc-seq"][1].file,
            { output = out, compression = compression.parse(params.compressions),
              ["io-mode"] = mode, ["staging-dir"] = scratch(mode) }):ok()
        local cell = workload.result(r, params)
        cell.io = mode
        if not r.stopped_at then cell.output_bytes = filesize(out) end
        record.cells[#record.cells + 1] = cell
        if r.stopped_at then stopped = true end
    end
    if stopped then return "stopped" end
end
