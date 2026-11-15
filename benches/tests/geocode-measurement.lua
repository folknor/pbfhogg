-- Mutate the instrument's measurement block, not the measured workload.
local host = { datasets = { fixture = {
    bbox = "0,0,1,1", pbf = { indexed = { file = "fixture.pbf" } },
} } }
local evidence = {
    elapsed_ms = "1", open_ms = "0", queries = "2", rounds = "2",
    us_per_query_best = "0.50", us_per_query_rounds = "0.60,0.50",
}
local function invoke(omit, replacement, extra)
    local lines = {}
    for key, value in pairs(evidence) do
        if key ~= omit then lines[#lines + 1] = key .. "=" .. (replacement and replacement[key] or value) end
    end
    if extra then lines[#lines + 1] = extra end
    return run {
        bench = "geocode-query", program = "pbfhogg-bench", host = host,
        params = { "--dataset", "fixture", "--queries", "2", "--rounds", "2" },
        files = { [project_root .. "/data/fixture.pbf"] = "" },
        calls = { {}, { stderr = table.concat(lines, "\n") .. "\n" } },
    }
end

test("geocode-query records a complete measurement block", function()
    local r = invoke()
    assert(r.outcome == "completed", r.reason)
    assert(r.record.child.us_per_query_best == 0.5)
end)

test("geocode-query ignores answer fields an older instrument printed", function()
    local r = invoke(nil, nil, "answer_digest=0123456789abcdef")
    assert(r.outcome == "completed", r.reason)
    assert(r.record.child.answer_digest == nil)
end)

for key in pairs(evidence) do
    test("geocode-query rejects missing " .. key, function()
        local r = invoke(key)
        assert(r.outcome == "failed", r.outcome)
        assert(r.reason:find("missing geocode field", 1, true), r.reason)
    end)
    test("geocode-query rejects duplicate " .. key, function()
        local r = invoke(nil, nil, key .. "=" .. evidence[key])
        assert(r.outcome == "failed", r.outcome)
        assert(r.reason:find("duplicate geocode field", 1, true), r.reason)
    end)
end

-- A malformed value, or a block that describes some other workload than
-- the one requested, is not a measurement of this one.
for key, value in pairs({
    elapsed_ms = "nan", open_ms = "-1", queries = "3", rounds = "3",
    us_per_query_best = "0.40", us_per_query_rounds = "0.50,",
}) do
    test("geocode-query rejects invalid or mismatched " .. key, function()
        local r = invoke(nil, { [key] = value })
        assert(r.outcome == "failed", r.outcome)
    end)
end
