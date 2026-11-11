-- Validation is a separate run; no call is a measured child.
local host = { datasets = { fixture = {
    bbox = "0,0,1,1", pbf = { indexed = { file = "fixture.pbf" } },
    osc = { ["9"] = { file = "fixture.osc.gz" } },
} } }
local function invoke(row, mismatch, no_calls)
    local params = { "--dataset", "fixture", "--row", row, "--tool", "pbfhogg", "--validate-only", "yes" }
    local files = { [project_root .. "/data/fixture.pbf"] = "" }
    if row == "apply-changes" then
        params[#params + 1] = "--osc"; params[#params + 1] = "9"
        files[project_root .. "/data/fixture.osc.gz"] = ""
    end
    return run {
        bench = "vs-osmium", program = "pbfhogg", host = host, params = params, files = files,
        calls = no_calls and {} or {
            { outputs = { ["pbfhogg.osm.pbf"] = "left" } },
            { outputs = { ["osmium.osm.pbf"] = "right" } },
            { stdout = "n1 v1 x0 y0\n" }, { stdout = "n1 v1 x0 y0\n" },
            { exit = mismatch and 1 or 0 },
        },
    }
end
for _, row in ipairs({ "sort", "extract", "apply-changes", "tags-filter-highway",
    "tags-filter-restaurant", "cat-way", "getid", "altw-sparse", "altw-external" }) do
    test("independent OPL validation for " .. row, function()
        local r = invoke(row, false)
        assert(r.outcome == "completed", r.reason)
        assert(r.record.validation == "osmium-opl-ordered-v1")
        assert(#r.calls == 5)
        for _, call in ipairs(r.calls) do assert(not call.measured) end
        for i = 3, 4 do
            assert(r.calls[i].argv[1] == "cat")
            local found = false
            for _, arg in ipairs(r.calls[i].argv) do
                if arg == "opl,locations_on_ways=true" then found = true end
            end
            assert(found, "way locations must be decoded too")
        end
    end)
    test("comparison mismatch refuses " .. row, function()
        local r = invoke(row, true)
        assert(r.outcome == "failed", r.outcome)
    end)
end
for _, row in ipairs({ "inspect", "inspect-tags", "inspect-tags-way" }) do
    test("refuse incompatible report validation for " .. row, function()
        local r = invoke(row, false, true)
        assert(r.outcome == "refused", r.outcome)
        assert(r.reason:find("PBF-producing row", 1, true), r.reason)
        assert(#r.calls == 0)
    end)
end
