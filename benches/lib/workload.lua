-- Shared validation and self-reported timing for the synthetic workloads.
local compression = require("compression")
local stops = require("stop")
local M = {}

function M.validate(params)
    if params["direct-io"] == "yes" then fail("synthetic workloads do not support --direct-io") end
    if params["io-uring"] == "yes" then fail("use --uring yes on merge-workload instead of --io-uring") end
    if params.compression then fail("use --compressions on write or merge instead of --compression") end
    if params.compressions then compression.parse(params.compressions) end
    stops.resolve(params.stop)
end

function M.result(r, params)
    if params.verbose == "yes" then
        print(r.stdout)
        print(r.stderr)
    end
    local cell = { summary = r.stderr }
    if r.stopped_at then
        cell.stopped_at = r.stopped_at
    else
        local elapsed = r.stderr:match("elapsed_ms=(%d+)")
        if not elapsed then fail("synthetic workload did not report elapsed_ms") end
        cell.elapsed_ms = tonumber(elapsed)
    end
    return cell
end

function M.finish(r, params)
    record.child = M.result(r, params)
    if r.stopped_at then return "stopped" end
end

return M
