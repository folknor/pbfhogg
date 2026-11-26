-- Shared point-box selection for the library and CLI query workloads.
local bboxes = require("bbox")
local M = {}

function M.bbox(params)
    local bbox = params.bbox or params.dataset.bbox
    if bbox == nil then
        fail("the dataset has no bbox in this host's config: pass --bbox")
    end
    return bbox, bboxes.parse(bbox)
end

return M
