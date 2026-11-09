local stops = require("stop")
local M = {}

function M.options(mark, stop)
    return { hotpath = mark, stop_at = stops.resolve(stop) }
end

return M
