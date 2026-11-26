-- geocode-query-cli: build the index with the library bench's default
-- settings, then measure one CLI call over the same SplitMix64 point set.
-- Point generation and index building are outside the measured child.
-- The call includes index open, point parsing, batch queries and JSON
-- output. Brokkr spools stdout. There is no warm-up or timed round loop.
--
--   brokkr bench --iterations 1 geocode-query-cli --dataset germany --queries 100000

local geocode = require("geocode")

function validate(params)
    geocode.bbox(params)
end

local LIMB = 65536
local INCREMENT = { 0x7c15, 0x7f4a, 0x79b9, 0x9e37 }
local MIX1 = { 0xe5b9, 0x1ce4, 0x476d, 0xbf58 }
local MIX2 = { 0x11eb, 0x1331, 0x49bb, 0x94d0 }
local floor = math.floor

-- Dellingr has f64 numbers and no bitwise operators. Use four little-endian
-- 16-bit limbs so the carries and products stay exact, including for seeds
-- above 2^53. The byte XOR table derives each entry from its upper seven
-- bits and the XOR of its low bits.
local function point_sampler(seed)
    local xor = {}
    for a = 0, 255 do
        for b = 0, 255 do
            local upper = xor[floor(a / 2) * 256 + floor(b / 2)] or 0
            xor[a * 256 + b] = 2 * upper + (a + b) % 2
        end
    end

    local state = { 0, 0, 0, 0 }
    -- Brokkr exposes typed params as their original text.
    for digit in seed:gmatch("%d") do
        local carry = tonumber(digit)
        for i = 1, 4 do
            local total = state[i] * 10 + carry
            state[i] = total % LIMB
            carry = floor(total / LIMB)
        end
    end

    -- All three finalizer shifts are 16 + k bits. The shifted limb comes
    -- from the next two limbs. Walk upward to keep those limbs untouched.
    local z, product = {}, {}
    local function xor_shift(divisor)
        for i = 1, 3 do
            local a = z[i]
            local b = floor(z[i + 1] / divisor) + ((z[i + 2] or 0) % divisor) * (LIMB / divisor)
            z[i] = xor[floor(a / 256) * 256 + floor(b / 256)] * 256
                + xor[(a % 256) * 256 + b % 256]
        end
    end

    local function multiply(by)
        local carry = 0
        for i = 1, 4 do
            local total = carry
            for j = 1, i do total = total + z[j] * by[i - j + 1] end
            product[i] = total % LIMB
            carry = floor(total / LIMB)
        end
        z, product = product, z
    end

    return function()
        local carry = 0
        for i = 1, 4 do
            local total = state[i] + INCREMENT[i] + carry
            state[i] = total % LIMB
            z[i] = state[i]
            carry = floor(total / LIMB)
        end
        xor_shift(16384) -- 30 bits
        multiply(MIX1)
        xor_shift(2048) -- 27 bits
        multiply(MIX2)
        xor_shift(32768) -- 31 bits
        local top53 = ((z[4] * LIMB + z[3]) * LIMB + z[2]) * 32 + floor(z[1] / 2048)
        return top53 / 9007199254740992
    end
end

function bench(params)
    local bbox, coords = geocode.bbox(params)
    local min_lon, min_lat, max_lon, max_lat = table.unpack(coords)
    local unit = point_sampler(params.seed)
    local lines = {}
    local bytes = 0
    for i = 1, tonumber(params.queries) do
        local lat = min_lat + unit() * (max_lat - min_lat)
        local lon = min_lon + unit() * (max_lon - min_lon)
        local line = string.format("%.17g %.17g\n",
            math.max(min_lat, math.min(lat, max_lat)),
            math.max(min_lon, math.min(lon, max_lon)))
        bytes = bytes + #line
        if bytes > 16 * 1024 * 1024 then
            fail("--queries produces a points file above Brokkr's 16 MiB string limit")
        end
        lines[i] = line
    end
    local points = output("points.txt")
    write(points, table.concat(lines))

    local index = output("geocode-index")
    binary("build-geocode-index", params.variant.file, {
        ["output-dir"] = index,
        ["tmp-dir"] = scratch("geocode"),
        ["allow-unindexed"] = true,
    }):ok()

    local r = binary_with({ measured = true, reads = { index, points }, hotpath = "query" },
        "geocode-query", index.path, points.path):ok()
    record.child = { bbox = bbox, queries = tonumber(params.queries) }
    if params.verbose == "yes" then print(r.stderr) end
end
