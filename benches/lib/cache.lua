local M = {}

function M.resident(path)
    local r = binary_with({ program = "fincore" }, "--bytes", "--noheadings", "--output", "RES", path):ok()
    local res = tonumber(r.stdout:match("%d+"))
    if res == nil then fail("fincore gave no resident size for " .. path) end
    return res
end

return M
