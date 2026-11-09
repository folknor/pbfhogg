-- Shared ordered expression decoding for comma-safe bench params.
-- expr-hex is an ASCII JSON string array encoded as hexadecimal bytes.
local M = {}

function M.expressions(params)
    if not params["expr-hex"] then return params.expr or {} end
    local value = params["expr-hex"]
    if #value == 0 or #value % 2 ~= 0 or value:find("[^%x]") then
        fail("--expr-hex must be hex-encoded JSON")
    end
    local decoded = value:gsub("%x%x", function(pair)
        if tonumber(pair, 16) > 127 then
            fail("--expr-hex must encode ASCII JSON; use JSON unicode escapes for non-ASCII")
        end
        return json.decode('"\\u00' .. pair .. '"')
    end)
    if not decoded:match("^%s*%[") then fail("--expr-hex must encode a JSON array") end
    local values = json.decode(decoded)
    if type(values) ~= "table" or json.encode(values):sub(1, 1) ~= "[" then
        fail("--expr-hex must encode a JSON array")
    end
    return values
end

return M
