-- Span-end aliases shared by validation and child options: FOO, -FOO,
-- FOO_END and -FOO_END all resolve to FOO_END. Use =FOO for an exact
-- point marker (or any exact marker name), without appending _END.
-- stop_at matches verbatim, with no runtime fallback. Brokkr owns the
-- required-stop postcondition: both its real and contract hosts reject a
-- child that exits or stops elsewhere. Keep that rule in the runner so
-- every stop-capable script receives the same enforcement.
local M = {}

function M.resolve(value)
    if value == nil then return nil end
    if not value:match("^%-?[%w_]+$") and not value:match("^=[%w_]+$") then
        fail("--stop must be FOO_END, FOO, -FOO, -FOO_END or =MARKER (exact)")
    end
    if value:sub(1, 1) == "=" then return value:sub(2) end
    local marker = value:gsub("^-", "")
    if marker:sub(-4) ~= "_END" then
        marker = marker .. "_END"
    end
    if marker == "_END" then
        fail("--stop must be FOO_END, FOO, -FOO, -FOO_END or =MARKER (exact)")
    end
    return marker
end

return M
