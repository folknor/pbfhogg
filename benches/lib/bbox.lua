-- A bbox as four numbers, for scripts that need its coordinates before the
-- child does. A bbox param is declared `bbox = true`, so brokkr has already
-- checked its syntax, finiteness and WGS84 range, and a dataset bbox is held
-- to the same rules; this adds the one rule brokkr's domain leaves out:
-- pbfhogg's Bbox::try_new refuses a minimum equal to its maximum, which
-- brokkr allows, and a script that would build an index or an extract before
-- the child refuses it checks that first. Callers keep the original text for
-- argv: pbfhogg quantizes edges from their decimal spelling.
--
-- brokkr trims Unicode whitespace around each coordinate before parsing;
-- dellingr's tonumber trims only ASCII whitespace, so a coordinate brokkr
-- accepted can still fail here, and that is refused by name rather than
-- read as nil.

local M = {}

-- value: lon_min,lat_min,lon_max,lat_max. Returns { lon_min, lat_min,
-- lon_max, lat_max } or fails naming what is wrong.
function M.parse(value)
    local coords = {}
    for part in (value .. ","):gmatch("(.-),") do
        local n = tonumber(part)
        if n == nil then fail("bbox coordinate '" .. part .. "' is not a number Lua can read") end
        coords[#coords + 1] = n
    end
    if #coords ~= 4 then fail("bbox must be four comma-separated coordinates, got " .. #coords) end
    if coords[1] >= coords[3] or coords[2] >= coords[4] then
        fail("bbox " .. value .. ": each minimum must be below its maximum")
    end
    return coords
end

return M
