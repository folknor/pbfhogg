-- The page-cache benches put brokkr's cold or warm on their one measured
-- call, over exactly the inputs that call reads, and check page_cache() in
-- the preflight. Every case gives responses for the workload calls only, so
-- a leftover dd or fincore child fails the case as a call with no response.

local DATA = project_root .. "/data/"

local function entry(file) return { file = file } end

local HOST = { datasets = {
    denmark = {
        pbf = { indexed = entry("dk.pbf"), raw = entry("dk-raw.pbf") },
        osc = { ["4705"] = entry("dk-4705.osc.gz") },
        snapshot = { later = { pbf = { indexed = entry("dk-later.pbf"), raw = entry("dk-later-raw.pbf") } } },
    },
} }

local function files(names)
    local t = {}
    for _, n in ipairs(names) do t[DATA .. n] = "" end
    return t
end

local function list(t) return t or {} end

-- Responses for a call that writes the PBF output, and for a differing
-- diff that writes its text output.
local function wrote() return { outputs = { ["out.osm.pbf"] = "" } } end
local function diffed() return { exit = 1, outputs = { ["diff.txt"] = "" } } end

-- The one measured call, with the given mode over exactly the given files,
-- and nothing in the other mode.
local function prepared(call, mode, names)
    local other = mode == "cold" and "warm" or "cold"
    assert(call.measured == true, "the call is not measured")
    assert(#list(call[other]) == 0, other .. " is not empty")
    local got = list(call[mode])
    assert(#got == #names, mode .. " lists " .. #got .. " files, want " .. #names)
    for i, n in ipairs(names) do
        assert(got[i] == DATA .. n, mode .. "[" .. i .. "] is " .. tostring(got[i]))
    end
end

local function unmeasured(call)
    assert(not call.measured, "a second call is measured")
    assert(#list(call.cold) == 0 and #list(call.warm) == 0, "a second call prepares the cache")
end

local function bench(name, params, names, calls, page_cache)
    return run {
        bench = name, program = "pbfhogg", params = params, host = HOST, files = files(names),
        calls = calls, page_cache = page_cache,
    }
end

local function refused_by(r, reason)
    assert(r.stage == "preflight", r.stage)
    assert(r.outcome == "refused", r.outcome)
    assert(string.find(r.reason, reason, 1, true), r.reason)
end

-- sort-cold

test("sort-cold sorts its input cold, measured", function()
    local r = bench("sort-cold", { "--dataset", "denmark" }, { "dk-raw.pbf" }, { wrote() })
    assert(r.outcome == "completed", r.reason)
    assert(#r.calls == 1, #r.calls)
    assert(r.calls[1].argv[1] == "sort", r.calls[1].argv[1])
    prepared(r.calls[1], "cold", { "dk-raw.pbf" })
end)

test("sort-cold refuses an input page_cache rules out", function()
    local r = bench("sort-cold", { "--dataset", "denmark" }, { "dk-raw.pbf" }, nil,
        { [DATA .. "dk-raw.pbf"] = "on tmpfs" })
    refused_by(r, "on tmpfs")
end)

-- dedupe-self

for _, mode in ipairs({ "cold", "warm" }) do
    test("dedupe-self merges its input " .. mode .. ", measured", function()
        local r = bench("dedupe-self", { "--dataset", "denmark", "--cache", mode }, { "dk.pbf" }, { wrote() })
        assert(r.outcome == "completed", r.reason)
        assert(#r.calls == 1, #r.calls)
        assert(r.calls[1].argv[1] == "cat", r.calls[1].argv[1])
        prepared(r.calls[1], mode, { "dk.pbf" })
    end)

    test("dedupe-self --cache " .. mode .. " refuses an input page_cache rules out", function()
        local r = bench("dedupe-self", { "--dataset", "denmark", "--cache", mode }, { "dk.pbf" }, nil,
            { [DATA .. "dk.pbf"] = "not observable" })
        refused_by(r, "not observable")
    end)
end

test("dedupe-self --check yes diffs unmeasured after the measured cat", function()
    local params = { "--dataset", "denmark", "--cache", "cold", "--check", "yes" }
    local r = bench("dedupe-self", params, { "dk.pbf" }, { wrote(),{ exit = 0 } })
    assert(r.outcome == "completed", r.reason)
    prepared(r.calls[1], "cold", { "dk.pbf" })
    assert(r.calls[2].argv[1] == "diff", r.calls[2].argv[1])
    unmeasured(r.calls[2])
    r = bench("dedupe-self", params, { "dk.pbf" }, { wrote(),{ exit = 1 } })
    assert(r.outcome == "failed", r.outcome)
    assert(string.find(r.reason, "differs from its input", 1, true), r.reason)
end)

-- dedupe-pair

local PAIR = { "dk.pbf", "dk-later.pbf" }

for _, mode in ipairs({ "cold", "warm" }) do
    test("dedupe-pair merges both inputs " .. mode .. ", measured", function()
        local r = bench("dedupe-pair", { "--dataset", "denmark", "--with", "later", "--cache", mode }, PAIR, { wrote() })
        assert(r.outcome == "completed", r.reason)
        assert(#r.calls == 1, #r.calls)
        prepared(r.calls[1], mode, PAIR)
    end)

    for _, f in ipairs(PAIR) do
        test("dedupe-pair --cache " .. mode .. " refuses " .. f .. " when page_cache rules it out", function()
            local r = bench("dedupe-pair", { "--dataset", "denmark", "--with", "later", "--cache", mode }, PAIR, nil,
                { [DATA .. f] = "refused " .. f })
            refused_by(r, "refused " .. f)
        end)
    end
end

test("dedupe-pair --check yes diffs unmeasured after the measured cat", function()
    local params = { "--dataset", "denmark", "--with", "later", "--cache", "warm", "--check", "yes" }
    local r = bench("dedupe-pair", params, PAIR, { wrote(),{ exit = 0 } })
    assert(r.outcome == "completed", r.reason)
    prepared(r.calls[1], "warm", PAIR)
    unmeasured(r.calls[2])
    r = bench("dedupe-pair", params, PAIR, { wrote(),{ exit = 1 } })
    assert(r.outcome == "failed", r.outcome)
    assert(string.find(r.reason, "differs from the base", 1, true), r.reason)
end)

-- diff-unindexed and diff-text

local RAW_PAIR = { "dk-raw.pbf", "dk-later-raw.pbf" }

test("diff-unindexed diffs both inputs cold, measured", function()
    local r = bench("diff-unindexed", { "--dataset", "denmark", "--new", "later" }, RAW_PAIR, { diffed() })
    assert(r.outcome == "completed", r.reason)
    assert(#r.calls == 1, #r.calls)
    prepared(r.calls[1], "cold", RAW_PAIR)
end)

for _, f in ipairs(RAW_PAIR) do
    test("diff-unindexed refuses " .. f .. " when page_cache rules it out", function()
        local r = bench("diff-unindexed", { "--dataset", "denmark", "--new", "later" }, RAW_PAIR, nil,
            { [DATA .. f] = "refused " .. f })
        refused_by(r, "refused " .. f)
    end)
end

test("diff-text diffs both inputs warm, measured", function()
    local r = bench("diff-text", { "--dataset", "denmark", "--new", "later" }, PAIR, { diffed() })
    assert(r.outcome == "completed", r.reason)
    assert(#r.calls == 1, #r.calls)
    prepared(r.calls[1], "warm", PAIR)
end)

for _, f in ipairs(PAIR) do
    test("diff-text refuses " .. f .. " when page_cache rules it out", function()
        local r = bench("diff-text", { "--dataset", "denmark", "--new", "later" }, PAIR, nil,
            { [DATA .. f] = "refused " .. f })
        refused_by(r, "refused " .. f)
    end)
end

-- vs-osmium

for _, tool in ipairs({ "pbfhogg", "osmium" }) do
    test("vs-osmium " .. tool .. " sorts its input cold, measured", function()
        local r = bench("vs-osmium", { "--dataset", "denmark", "--row", "sort", "--tool", tool }, { "dk.pbf" }, { wrote() })
        assert(r.outcome == "completed", r.reason)
        assert(#r.calls == 1, #r.calls)
        prepared(r.calls[1], "cold", { "dk.pbf" })
        if tool == "osmium" then assert(r.calls[1].program == "osmium", tostring(r.calls[1].program)) end
    end)

    test("vs-osmium " .. tool .. " apply-changes takes the PBF and the OSC cold", function()
        local names = { "dk.pbf", "dk-4705.osc.gz" }
        local params = { "--dataset", "denmark", "--row", "apply-changes", "--osc", "4705", "--tool", tool }
        local r = bench("vs-osmium", params, names, { wrote() })
        assert(r.outcome == "completed", r.reason)
        prepared(r.calls[1], "cold", names)
    end)
end

for _, f in ipairs({ "dk.pbf", "dk-4705.osc.gz" }) do
    test("vs-osmium refuses " .. f .. " when page_cache rules it out", function()
        local params = { "--dataset", "denmark", "--row", "apply-changes", "--osc", "4705", "--tool", "pbfhogg" }
        local r = bench("vs-osmium", params, { "dk.pbf", "dk-4705.osc.gz" }, nil, { [DATA .. f] = "refused " .. f })
        refused_by(r, "refused " .. f)
    end)
end
