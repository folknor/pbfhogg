-- Mutate the legacy instrument evidence, not the measured workload.
local host = { datasets = { fixture = {
    bbox = "0,0,1,1", pbf = { indexed = { file = "fixture.pbf" } },
} } }
local evidence = {
    elapsed_ms = "1", open_ms = "0", queries = "2", rounds = "2",
    us_per_query_best = "0.50", us_per_query_rounds = "0.60,0.50",
    admin_answered = "1", admin_matches = "2", addresses = "1",
    streets = "0", interpolations = "0", admin_digest = "0123456789abcdef",
}
local function invoke(omit, replacement, extra)
    local lines = {}
    for key, value in pairs(evidence) do
        if key ~= omit then lines[#lines + 1] = key .. "=" .. (replacement and replacement[key] or value) end
    end
    if extra then lines[#lines + 1] = extra end
    return run {
        bench = "geocode-query", program = "pbfhogg", host = host,
        params = { "--dataset", "fixture", "--queries", "2", "--rounds", "2" },
        files = { [project_root .. "/data/fixture.pbf"] = "" },
        calls = { {}, { stderr = table.concat(lines, "\n") .. "\n" } },
    }
end

test("geocode accepts complete consistent legacy evidence", function()
    local r = invoke()
    assert(r.outcome == "completed", r.reason)
end)

for key in pairs(evidence) do
    test("geocode rejects missing " .. key, function()
        local r = invoke(key)
        assert(r.outcome == "failed", r.outcome)
        assert(r.reason:find("missing geocode field", 1, true), r.reason)
    end)
    test("geocode rejects duplicate " .. key, function()
        local r = invoke(nil, nil, key .. "=" .. evidence[key])
        assert(r.outcome == "failed", r.outcome)
        assert(r.reason:find("duplicate geocode field", 1, true), r.reason)
    end)
end

for key, value in pairs({
    elapsed_ms = "nan", open_ms = "-1", queries = "3", rounds = "3",
    admin_answered = "3", admin_matches = "0", addresses = "3",
    streets = "3", interpolations = "3", admin_digest = "123",
    us_per_query_best = "0.40", us_per_query_rounds = "0.50,",
}) do
    test("geocode rejects invalid or inconsistent " .. key, function()
        local r = invoke(nil, { [key] = value })
        assert(r.outcome == "failed", r.outcome)
    end)
end
