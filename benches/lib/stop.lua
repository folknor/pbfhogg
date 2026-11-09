-- Span-end spelling shared by validation and child options.
local M = {}

function M.resolve(value)
    if value == nil then return nil end
    local marker = value:gsub("^-", "")
    if marker:sub(-4) ~= "_END" then marker = marker .. "_END" end
    if not value:match("^%-?[%w_]+$") or marker == "_END" then
        fail("--stop must be a marker name: FOO_END, FOO, -FOO or -FOO_END")
    end
    return marker
end

return M
