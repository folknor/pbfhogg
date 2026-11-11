-- The runner owns required stops for every caller of stop_at. Exercise it
-- through a real script and the shared alias resolver.
local FILE = project_root .. "/data/stop.pbf"
local HOST = { datasets = { denmark = { pbf = { indexed = { file = "stop.pbf" } } } } }

local function check(marker, ending)
    return run {
        bench = "index-integrity", program = "pbfhogg", host = HOST,
        params = { "--dataset", "denmark", "--stop", marker },
        files = { [FILE] = "" }, calls = { ending },
    }
end

test("a reached span alias reports its resolved stop", function()
    local r = check("CHECK_INDEX", { stopped_at = "CHECK_INDEX_END" })
    assert(r.outcome == "completed", r.reason)
    assert(r.record.stopped_at == "CHECK_INDEX_END", tostring(r.record.stopped_at))
end)

test("a missing stop cannot turn a phase measurement into a full run", function()
    local r = check("CHECK_INDEX", { exit = 0 })
    assert(r.outcome == "failed", r.outcome)
    assert(string.find(r.reason, "stop_at marker CHECK_INDEX_END", 1, true), r.reason)
end)

test("a renamed exact marker cannot accept normal completion", function()
    local r = check("=RENAMED_POINT", { exit = 0 })
    assert(r.outcome == "failed", r.outcome)
    assert(string.find(r.reason, "stop_at marker RENAMED_POINT", 1, true), r.reason)
end)
