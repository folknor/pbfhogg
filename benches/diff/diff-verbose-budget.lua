-- diff-verbose-budget: diff -v over synthetic modified ways sized against
-- VERBOSE_SEQUENCE_DIFF_MAX_WORK and VERBOSE_SEQUENCE_DIFF_MAX_EDITS
-- (crates/lib/src/tunables.rs). Each of --ways ways per shape has 30,000
-- refs on both sides:
--
--   disjoint:  old refs 1..30000, new refs 30001..60000. The search runs out
--              of budget and prints one "refs: ... replaced" line.
--   scattered: every 60th ref changed (500 changes, 1000 edits, at the edit
--              cap). The search finishes and prints 500 -ref then 500 +ref.
--
-- Preparation is untimed and uses only existing commands: getid of an ID
-- the dataset is not expected to hold (a hit only adds a common element,
-- which -c hides) gives a base carrying the dataset's sorted header;
-- apply-changes --force of a written create OSC gives the old side, and
-- apply-changes of a modify OSC on that gives the new side. Refs name
-- nodes that do not exist, which neither command needs. Only the diff
-- child is measured. Its wall includes process start, reading both small
-- files, decoding the refs and formatting, not only the sequence search;
-- --ways amortises the fixed part. -j 1 because these compressible ways
-- fit one frame, so more workers would not schedule more searches.
--
-- The diff must exit 1 and print, under every way, exactly the replacement
-- or itemized script its shape demands; anything else fails the iteration.
--
--   brokkr bench --iterations 5 diff-verbose-budget
--   brokkr bench diff-verbose-budget --ways 12
local REFS = 30000
local STEP = 60
-- --ways is capped at 12 in brokkr.d/benches.toml: each OSC is built as one
-- Lua string, which brokkr caps at 16 MiB.

local function new_ref(shape, i)
    -- i is the zero-based position; old refs are i + 1.
    if shape == "disjoint" then return i + 1 + REFS end
    if i % STEP == 0 then return i + 1 + REFS end
    return i + 1
end

local function way_xml(parts, id, version, shape)
    parts[#parts + 1] = '<way id="' .. id .. '" version="' .. version .. '">'
    for i = 0, REFS - 1 do
        local ref = i + 1
        if shape then ref = new_ref(shape, i) end
        parts[#parts + 1] = '<nd ref="' .. ref .. '"/>'
    end
    parts[#parts + 1] = "</way>"
end

local function osc(action, ways, modified)
    local parts = { '<?xml version="1.0" encoding="UTF-8"?>\n<osmChange version="0.6">\n<' .. action .. ">" }
    for w = 1, 2 * ways do
        local shape = nil
        if modified then
            shape = "scattered"
            if w <= ways then shape = "disjoint" end
        end
        way_xml(parts, w, modified and 2 or 1, shape)
    end
    parts[#parts + 1] = "</" .. action .. ">\n</osmChange>\n"
    return table.concat(parts)
end

local function expected(w, ways)
    local lines = { "*w" .. w .. " v1 -> v2" }
    if w <= ways then
        lines[2] = "  refs: " .. REFS .. " -> " .. REFS .. ", old @0.." .. REFS
            .. " -> new @0.." .. REFS .. " replaced (details omitted)"
        return lines
    end
    for i = 0, REFS - 1, STEP do lines[#lines + 1] = "  -ref @" .. i .. " n" .. (i + 1) end
    for i = 0, REFS - 1, STEP do lines[#lines + 1] = "  +ref @" .. i .. " n" .. (i + 1 + REFS) end
    return lines
end

local function check(text, ways)
    local want = {}
    for w = 1, 2 * ways do
        for _, line in ipairs(expected(w, ways)) do want[#want + 1] = line end
    end
    local n = 0
    for line in string.gmatch(text, "([^\n]*)\n") do
        n = n + 1
        if line ~= want[n] then
            fail("diff output line " .. n .. " is '" .. line .. "', expected '" .. tostring(want[n]) .. "'")
        end
    end
    if n ~= #want then
        fail("diff printed " .. n .. " lines, expected " .. #want)
    end
end

function bench(params)
    local ways = tonumber(params.ways)
    local base = output("base.osm.pbf")
    binary("getid", params.variant.file, "n1", { output = base, overwrite = true }):ok()
    local create = output("create.osc")
    write(create, osc("create", ways, false))
    local old = output("old.osm.pbf")
    binary("apply-changes", base, create, { output = old, overwrite = true, force = true }):ok()
    local modify = output("modify.osc")
    write(modify, osc("modify", ways, true))
    local new = output("new.osm.pbf")
    binary("apply-changes", old, modify, { output = new, overwrite = true }):ok()

    local out = output("diff.txt")
    local d = binary_with({ measured = true }, "diff", old, new, {
        output = out, overwrite = true, verbose = true, ["suppress-common"] = true, jobs = 1,
    })
    record.diff_exit = d.exit
    record.ways = ways
    if d.exit ~= 1 then
        fail("diff exited " .. tostring(d.exit) .. ", expected 1: " .. d.stderr)
    end
    record.output_bytes = filesize(out)
    record.summary = d.stderr
    check(read(out), ways)
end
