-- encode-workload: typed write's synthetic workload, without cache preparation.
-- Builds the pbfhogg-cli package's pbfhogg bin, not an example or harness.
-- Each selected matrix value is an arm with one child, except --uring yes,
-- which runs buffered then uring children on the same build for merge.
-- Uses the child's own library workload; no command-layer substitute.
-- Child elapsed_ms is retained separately; the measured child's wall is
-- the iteration result, not the typed runner's self-reported wall.
-- Outputs and staging live under host scratch's bench-iterations directory.
-- Stops return a stopped verdict; their outputs are incomplete.
-- Build overrides, commits and iteration counts belong to the bench runner.
-- Both writers encode into a byte sink; no PBF output is produced.
--
--   brokkr bench --dry-run encode-workload
--   brokkr bench --alloc encode-workload

local workload = require("workload")
local profiling = require("profiling")
local compression = require("compression")

function validate(params)
    workload.validate(params)
end

function bench(params)
    local codec = compression.parse(params.compressions)
    if codec == "zlib" then codec = "zlib:6" end
    if codec == "zstd" then codec = "zstd:3" end
    local options = profiling.options("write", params.stop)
    options.measured = true
    local r = binary_with(options,
        "bench-write", params.variant.file,
        { compression = codec, writer = params.writer }):ok()
    return workload.finish(r, params)
end
