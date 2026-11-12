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
test("refuse report validation for inspect", function()
    local r = invoke("inspect", false, true)
    assert(r.outcome == "refused", r.outcome)
    assert(r.reason:find("no osmium report", 1, true), r.reason)
    assert(#r.calls == 0)
end)

-- The inspect tags rows: pbfhogg's stdout, then osmium's.
local function report(row, ours, theirs)
    return run {
        bench = "vs-osmium", program = "pbfhogg", host = host,
        params = { "--dataset", "fixture", "--row", row, "--tool", "pbfhogg", "--validate-only", "yes" },
        files = { [project_root .. "/data/fixture.pbf"] = "" },
        calls = { { stdout = ours }, { stdout = theirs } },
    }
end
local ours = '9\t"highway"\n4\t"say \\"hi\\"\\\\"\n2\t"tab\\there\\nnl"\n'
local theirs = '9\t"highway"\n4\t"say ""hi""\\"\n2\t"tab\there\nnl"\n'
for _, row in ipairs({ "inspect-tags", "inspect-tags-way" }) do
    test("report rows decode equal across quoting for " .. row, function()
        local r = report(row, ours, theirs)
        assert(r.outcome == "completed", r.reason)
        assert(r.record.validation == "tags-count-rows-ordered-v1")
        assert(#r.calls == 2)
        for _, call in ipairs(r.calls) do assert(not call.measured) end
        assert(r.calls[2].argv[1] == "tags-count")
    end)
    test("report count difference fails " .. row, function()
        local r = report(row, '9\t"highway"\n', '8\t"highway"\n')
        assert(r.outcome == "failed", r.outcome)
        assert(r.reason:find("row 1 differs", 1, true), r.reason)
    end)
    test("report order difference fails " .. row, function()
        local r = report(row, '1\t"a"\n1\t"b"\n', '1\t"b"\n1\t"a"\n')
        assert(r.outcome == "failed", r.outcome)
    end)
    test("report missing row fails " .. row, function()
        local r = report(row, '1\t"a"\n1\t"b"\n', '1\t"a"\n')
        assert(r.outcome == "failed", r.outcome)
        assert(r.reason:find("no row", 1, true), r.reason)
    end)
    test("report malformed rows fail " .. row, function()
        for _, bad in ipairs({ '1\t"a"\n\n', '1\t"a"\t"v"\n', '1\t"a\n', '1\t"a"x\n', 'x\t"a"\n', '1\t"a"' }) do
            assert(report(row, bad, '1\t"a"\n').outcome == "failed", bad)
            assert(report(row, '1\t"a"\n', bad).outcome == "failed", bad)
        end
    end)
end
