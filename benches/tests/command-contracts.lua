-- Argument-heavy scripts: use synthetic files and mocked child responses only.
local DATA = project_root .. "/data/"
local HOST = { datasets = { fixture = {
    pbf = { indexed = { file = "base.pbf" } },
    osc = { ["9"] = { file = "9.osc.gz" }, ["10"] = { file = "10.osc.gz" } },
} } }
local function invoke(name, args, calls)
    local params = { "--dataset", "fixture" }
    for _, arg in ipairs(args) do params[#params + 1] = arg end
    -- files holds only the inputs this arm binds.
    local files = {}
    local osc_input = false
    for i, arg in ipairs(params) do
        if arg == "--input-kind" and params[i + 1] == "osc" then osc_input = true end
    end
    if name ~= "merge-diffs" and not osc_input then files[DATA .. "base.pbf"] = "base" end
    for i, arg in ipairs(params) do
        if arg == "--osc" or (arg == "--input" and osc_input) then
            for seq in params[i + 1]:gmatch("%d+") do
                files[DATA .. seq .. ".osc.gz"] = seq
            end
        end
    end
    return run { bench = name, program = "pbfhogg", host = HOST, params = params,
        files = files, calls = calls }
end
local function has(argv, key, value)
    for i, arg in ipairs(argv) do
        if arg == key and (value == nil or argv[i + 1] == value) then return true end
    end
    return false
end
local function contains(argv, key, value)
    assert(has(argv, key, value), table.concat(argv, " "))
end

local cases = {
    { name = "geocode-build", command = "build-geocode-index",
        args = { "--direct-io", "yes" },
        flags = { { "--force" }, { "--direct-io" }, { "--output-dir" }, { "--tmp-dir" } } },
    { name = "id-remap", command = "renumber",
        args = { "--direct-io", "yes", "--compression", "zstd:1" },
        flags = { { "--direct-io" }, { "--compression", "zstd:1" }, { "--output" } } },
    { name = "id-select", command = "getid",
        args = { "--add-referenced", "yes", "--direct-io", "yes" },
        flags = { { "--add-referenced" }, { "--direct-io" }, { "--output" } } },
    { name = "parent-select", command = "getparents",
        args = { "--compression", "zstd:1" },
        flags = { { "--compression", "zstd:1" }, { "--output" } } },
    { name = "perturb-pbf", command = "degrade", out = "degrade.pbf",
        args = { "--strip-indexdata", "yes", "--strip-locations", "yes", "--unsort", "yes", "--io-uring", "yes" },
        flags = { { "--strip-indexdata" }, { "--strip-locations" }, { "--unsort" }, { "--io-uring" }, { "--overwrite" } } },
    { name = "reencode-pbf", command = "repack", out = "repack.pbf",
        args = { "--elements-per-blob", "17", "--force-repack", "yes", "--io-uring", "yes" },
        flags = { { "--elements-per-blob", "17" }, { "--force" }, { "--io-uring" }, { "--overwrite" } } },
    { name = "region-extract", command = "extract", out = "smart.osm.pbf",
        args = { "--bbox", "0,0,1,1", "--strategy", "smart", "--io-uring", "yes" },
        flags = { { "-b=0,0,1,1" }, { "--smart" }, { "--io-uring" }, { "--overwrite" } } },
    { name = "sort-pbf", command = "sort", out = "sorted.pbf",
        args = { "--force-sort", "yes", "--direct-io", "yes", "--io-uring", "yes", "--compression", "zstd:1" },
        flags = { { "--force" }, { "--direct-io" }, { "--io-uring" }, { "--compression", "zstd:1" } } },
    { name = "time-snapshot", command = "time-filter",
        args = { "--direct-io", "yes", "--compression", "zstd:1" },
        flags = { { "2024-01-01T00:00:00Z" }, { "--direct-io" }, { "--compression", "zstd:1" } } },
    { name = "copy-pbf", command = "cat", out = "cat.pbf",
        args = { "--type", "way", "--clean", "yes", "--direct-io", "yes", "--compression", "zstd:1" },
        flags = { { "--type", "way" }, { "--clean", "version" }, { "--direct-io" }, { "--compression", "zstd:1" } } },
    { name = "apply-diff", command = "apply-changes", out = "applied.osm.pbf",
        args = { "--osc", "10", "--locations-on-ways", "yes", "--jobs", "2", "--io-uring", "yes" },
        flags = { { "--locations-on-ways" }, { "--jobs", "2" }, { "--io-uring" } } },
    { name = "embed-locations", command = "add-locations-to-ways", out = "locations.osm.pbf",
        args = { "--index-type", "external", "--force-altw", "yes", "--inject-prepass", "yes",
            "--keep-untagged-nodes", "yes", "--ignore-missing-nodes", "yes", "--io-uring", "yes" },
        flags = { { "--index-type", "external" }, { "--force" }, { "--inject-prepass" },
            { "--keep-untagged-nodes" }, { "--ignore-missing-nodes" }, { "--io-uring" }, { "--tmp-dir" } } },
    { name = "tag-select", command = "tags-filter",
        args = { "--filter", "w/highway=primary,secondary", "--invert-match", "yes", "--remove-tags", "yes", "--jobs", "2" },
        flags = { { "--invert-match" }, { "--remove-tags" }, { "--jobs", "2" } } },
    { name = "file-info", command = "inspect",
        args = { "--mode", "tags", "--type", "way", "--expr", "a=b,c", "--expr", "x=y", "--jobs", "2" },
        flags = { { "--type", "way" }, { "--min-count", "999999999" }, { "--jobs", "2" } } },
    { name = "merge-diffs", command = "merge-changes", out = "merged.osc.gz",
        args = { "--osc", "10", "--osc", "9" }, flags = { { "--tmp-dir" }, { "--overwrite" } } },
}
for _, case in ipairs(cases) do
    test(case.name .. " forwards selected arguments and records completion", function()
        local outputs = {}
        if case.out then outputs[case.out] = "abc" end
        local r = invoke(case.name, case.args, { { stderr = "summary", outputs = outputs } })
        assert(r.outcome == "completed", r.reason)
        assert(#r.calls == 1)
        local argv = r.calls[1].argv
        assert(argv[1] == case.command)
        for _, flag in ipairs(case.flags) do contains(argv, table.unpack(flag)) end
        if case.name == "region-extract" then
            assert(#r.record.strategies == 1)
            assert(r.record.strategies[1].summary == "summary")
            assert(r.record.strategies[1].output_bytes[1] == 3)
        else
            assert(r.record.summary == "summary")
            if case.out then assert(r.record.output_bytes == 3) end
        end
        if case.name == "id-select" or case.name == "parent-select" then
            assert(argv[3] == "n115722" and argv[4] == "n115723")
            assert(r.record.ids[1] == "n115722")
        end
        if case.name == "merge-diffs" then
            assert(argv[2] == DATA .. "10.osc.gz" and argv[3] == DATA .. "9.osc.gz")
        elseif case.name == "apply-diff" then
            assert(argv[2] == DATA .. "base.pbf" and argv[3] == DATA .. "10.osc.gz")
        elseif case.name == "tag-select" then
            assert(argv[3] == "w/highway=primary,secondary")
            assert(not has(argv, "--input-kind"))
        elseif case.name == "file-info" then
            assert(argv[2] == "tags" and argv[4] == "a=b,c" and argv[5] == "x=y")
        end
    end)
    test(case.name .. " stops without reading incomplete output", function()
        local args = { table.unpack(case.args) }
        args[#args + 1] = "--stop"; args[#args + 1] = "=CONTRACT_POINT"
        local r = invoke(case.name, args, { { stopped_at = "CONTRACT_POINT" } })
        assert(r.outcome == "completed", r.reason)
        assert(r.verdict == "stopped")
        if case.name == "region-extract" then
            assert(r.record.strategies[1].stopped_at == "CONTRACT_POINT")
            assert(r.record.strategies[1].output_bytes == nil)
        else
            assert(r.record.stopped_at == "CONTRACT_POINT")
        end
        assert(r.record.output_bytes == nil)
        assert(r.calls[1].stop_at == "CONTRACT_POINT")
    end)
    test(case.name .. " rejects completion before the requested stop", function()
        local args = { table.unpack(case.args) }
        args[#args + 1] = "--stop"; args[#args + 1] = "=CONTRACT_POINT"
        local r = invoke(case.name, args, { { exit = 0 } })
        assert(r.outcome == "failed", r.outcome)
        assert(r.reason:find("stop_at marker CONTRACT_POINT", 1, true), r.reason)
    end)
    test(case.name .. " rejects child failure", function()
        local r = invoke(case.name, case.args, { { exit = 2, stderr = "child failed" } })
        assert(r.outcome == "failed", r.outcome)
    end)
end

for _, case in ipairs({
    { "geocode-build", { "--io-uring", "yes" }, "no io_uring writer" },
    { "geocode-build", { "--compression", "none" }, "writes no PBF" },
    { "perturb-pbf", {}, "at least one" },
    { "id-select", { "--add-referenced", "yes", "--invert", "yes" }, "mutually exclusive" },
    { "copy-pbf", { "--dedupe", "yes", "--clean", "yes" }, "takes neither" },
    { "copy-pbf", { "--io-uring", "yes" }, "requires --dedupe" },
    { "tag-select", { "--omit-referenced", "yes", "--jobs", "2" }, "--omit-referenced refuses" },
    { "tag-select", { "--input-kind", "osc", "--input", "9", "--remove-tags", "yes" }, "OSC input refuses" },
    { "file-info", { "--mode", "metadata", "--jobs", "2" }, "--jobs requires" },
    { "merge-diffs", { "--osc", "9", "--compression", "none" }, "writes OSC" },
}) do
    test(case[1] .. " refuses invalid combination " .. case[3], function()
        local r = invoke(case[1], case[2], {})
        assert(r.outcome == "refused", r.reason)
        assert(r.reason:find(case[3], 1, true), r.reason)
        assert(#r.calls == 0)
    end)
end

test("copy-pbf dedupe repeats the input and enables uring", function()
    local r = invoke("copy-pbf", { "--dedupe", "yes", "--io-uring", "yes" },
        { { outputs = { ["cat.pbf"] = "" } } })
    assert(r.outcome == "completed", r.reason)
    local argv = r.calls[1].argv
    assert(argv[2] == DATA .. "base.pbf" and argv[3] == argv[2])
    contains(argv, "--dedupe"); contains(argv, "--io-uring")
end)

test("tag-select OSC keeps the expression intact and selects OSC output", function()
    local r = invoke("tag-select", { "--input-kind", "osc", "--input", "9", "--filter", "n/name=a,b", "--jobs", "2" }, { {} })
    assert(r.outcome == "completed", r.reason)
    local argv = r.calls[1].argv
    assert(argv[2] == DATA .. "9.osc.gz" and argv[3] == "n/name=a,b")
    contains(argv, "--input-kind", "osc"); contains(argv, "--jobs", "2")
    for i, arg in ipairs(argv) do
        if arg == "--output" then assert(argv[i + 1]:match("filtered%.osc%.gz$")) end
    end
end)

for _, args in ipairs({ { "--osc", "9" }, { "--osc", "9..10", "--simplify", "yes" } }) do
    test("merge-diffs omits unused spill scratch " .. table.concat(args, " "), function()
        local r = invoke("merge-diffs", args, { { outputs = { ["merged.osc.gz"] = "" } } })
        assert(r.outcome == "completed", r.reason)
        assert(not has(r.calls[1].argv, "--tmp-dir"))
        if #args > 2 then contains(r.calls[1].argv, "--simplify") end
    end)
end
