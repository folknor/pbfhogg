local host = { datasets = { fixture = {
    bbox = "0,0,1,1", pbf = { indexed = { file = "fixture.pbf" } },
    osc = { ["9"] = { file = "fixture.osc.gz" } },
} } }
local function has(argv, value, following)
    for i, arg in ipairs(argv) do
        if arg == value and (following == nil or argv[i + 1] == following) then return true end
    end
    return false
end
for _, row in ipairs({ "sort", "extract", "apply-changes", "tags-filter-highway",
    "tags-filter-restaurant", "cat-way", "getid", "altw-sparse", "altw-external" }) do
    for _, tool in ipairs({ "pbfhogg", "osmium" }) do
        test("comparison pins compression for " .. row .. " " .. tool, function()
            local params = { "--dataset", "fixture", "--row", row, "--tool", tool }
            local files = { [project_root .. "/data/fixture.pbf"] = "" }
            if row == "apply-changes" then
                params[#params + 1] = "--osc"; params[#params + 1] = "9"
                files[project_root .. "/data/fixture.osc.gz"] = ""
            end
            local r = run {
                bench = "vs-osmium", program = "pbfhogg", host = host, params = params,
                files = files,
                calls = { { outputs = { ["out.osm.pbf"] = "" } } },
            }
            assert(r.outcome == "completed", r.reason)
            local argv = r.calls[1].argv
            if tool == "pbfhogg" then
                assert(has(argv, "--compression", "zlib:6"), table.concat(argv, " "))
            else
                assert(has(argv, "--output-format", "pbf,pbf_compression=zlib,pbf_compression_level=6"), table.concat(argv, " "))
                assert(has(argv, "--fsync"), table.concat(argv, " "))
                if row == "extract" then assert(has(argv, "--strategy", "complete_ways")) end
                if row:match("^altw") then assert(has(argv, "--keep-member-nodes")) end
            end
        end)
    end
end
