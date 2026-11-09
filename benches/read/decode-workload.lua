-- decode-workload: typed read's synthetic workload, without cache preparation.
-- Builds the pbfhogg-cli package's pbfhogg bin, not an example or harness.
-- Each selected matrix value is an arm with one child, except --uring yes,
-- which runs buffered then uring children on the same build for merge.
-- Uses the child's own library workload; no command-layer substitute.
-- Child elapsed_ms is retained separately: iteration wall includes launch,
-- setup and teardown, and is not the typed runner's self-reported wall.
-- Outputs and staging live under host scratch's bench-iterations directory.
-- Stops return a stopped verdict; their outputs are incomplete.
-- Build overrides, commits and iteration counts belong to the bench runner.
--
--   brokkr bench --dry-run decode-workload
--   brokkr bench --alloc decode-workload

local workload = require("workload")
local profiling = require("profiling")

function validate(params)
    workload.validate(params)
end

function bench(params)
    local r = binary_with(profiling.options("read", params.stop),
        "bench-read", params.variant.file, { mode = params.modes }):ok()
    return workload.finish(r, params)
end
