-- Shared general diff workload. Child wall, rather than iteration wall,
-- excludes merge preparation. No forced page-cache state.
local profiling = require("profiling")
local stops = require("stop")
local M = {}

function M.validate(params)
    stops.resolve(params.stop)
    if params.jobs then
        local digits = params.jobs:gsub("^%+", "")
        local n = digits:gsub("^0+", "")
        if n == "" then n = "0" end
        if not digits:match("^%d+$") or #n > 20
            or (#n == 20 and n > "18446744073709551615") then
            fail("--jobs must be an unsigned 64-bit integer")
        end
    end
end

-- Match the typed runner's first-OsmData-BlobHeader probe, rather than
-- guessing from the variant name or scanning every blob with inspect.
function M.unindexed(input)
    local r = binary_with({ program = "python3" }, "-c", "\n"
        .. "import sys\n"
        .. "\n"
        .. "def varint(data, pos):\n"
        .. "    value = shift = 0\n"
        .. "    while True:\n"
        .. "        b = data[pos]; pos += 1\n"
        .. "        value |= (b & 127) << shift\n"
        .. "        if b < 128: return value, pos\n"
        .. "        shift += 7\n"
        .. "\n"
        .. "def fields(data):\n"
        .. "    pos = 0\n"
        .. "    result = {}\n"
        .. "    while pos < len(data):\n"
        .. "        tag, pos = varint(data, pos)\n"
        .. "        wire = tag & 7\n"
        .. "        if wire == 0:\n"
        .. "            value, pos = varint(data, pos)\n"
        .. "        elif wire == 2:\n"
        .. "            size, pos = varint(data, pos)\n"
        .. "            value = data[pos:pos+size]; pos += size\n"
        .. "        elif wire in (1, 5):\n"
        .. "            pos += 8 if wire == 1 else 4\n"
        .. "            continue\n"
        .. "        else:\n"
        .. "            raise ValueError(\"invalid BlobHeader wire type\")\n"
        .. "        result[tag >> 3] = value\n"
        .. "    return result\n"
        .. "\n"
        .. "with open(sys.argv[1], \"rb\") as f:\n"
        .. "    while True:\n"
        .. "        size = f.read(4)\n"
        .. "        if not size:\n"
        .. "            print(\"indexed\"); break\n"
        .. "        n = int.from_bytes(size, \"big\")\n"
        .. "        if len(size) != 4 or not 0 < n <= 65536:\n"
        .. "            raise ValueError(\"invalid BlobHeader length\")\n"
        .. "        header = fields(f.read(n))\n"
        .. "        if header[1] == b\"OSMData\":\n"
        .. "            print(\"indexed\" if 2 in header else \"raw\"); break\n"
        .. "        f.seek(header[3], 1)\n", input):ok()
    return r.stdout:match("^raw") ~= nil
end

function M.run(params, old, new, want)
    local out = output(params.format == "osc" and "diff.osc" or "diff.txt")
    local flags = { output = out, overwrite = true, jobs = params.jobs, type = params.type,
        ["direct-io"] = params["direct-io"] == "yes" }
    if params.format == "osc" then
        flags.format = "osc"
        flags["tmp-dir"] = scratch("diff")
    else
        flags["suppress-common"] = true
    end
    local r = binary_with(profiling.options("diff", params.stop), "diff", old, new, flags)
    record.summary = r.stderr
    if params.verbose == "yes" then print(r.stdout); print(r.stderr) end
    if r.stopped_at then
        record.stopped_at = r.stopped_at
        return "stopped"
    end
    if r.exit ~= 0 and (r.exit ~= 1 or not since("854f9465")) then
        fail("diff ended with exit " .. tostring(r.exit) .. ", signal " .. tostring(r.signal) .. ": " .. r.stderr)
    end
    local verdict = r.exit == 0 and "same" or "differs"
    if want and want ~= verdict then fail("diff answered " .. verdict .. ", declared " .. want) end
    record.diff_exit = r.exit
    record.output_bytes = filesize(out)
    return verdict
end

return M
