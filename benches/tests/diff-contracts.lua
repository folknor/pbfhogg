-- Exercise the measured verdict separately from apply-changes preparation.
local DATA = project_root .. "/data/"
local function invoke(name, args, calls, want, trusted)
    local params = { "--dataset", "fixture" }
    for _, arg in ipairs(args) do params[#params + 1] = arg end
    local files = { [DATA .. "base.pbf"] = "base" }
    if name == "diff-pair" then files[DATA .. "later.pbf"] = "later" else files[DATA .. "9.osc.gz"] = "osc" end
    return run { bench = name, program = "pbfhogg", params = params,
        host = { datasets = { fixture = {
            expect = { ["diff-snapshots"] = { ["00000000000000000000000000000001+00000000000000000000000000000002"] = want } },
            pbf = { indexed = { file = "base.pbf", xxhash = "00000000000000000000000000000002", expect = { diff = { ["9"] = want } } } },
            osc = { ["9"] = { file = "9.osc.gz" } },
            snapshot = { later = { pbf = { indexed = { file = "later.pbf", xxhash = "00000000000000000000000000000001" } } } },
        } } }, files = files, calls = calls,
        since = { ["854f9465"] = trusted ~= false } }
end
local function has(argv, key, value)
    for i, arg in ipairs(argv) do
        if arg == key and (value == nil or argv[i + 1] == value) then return true end
    end
    return false
end
local PAIR = { "--from", "base", "--to", "later" }
for _, format in ipairs({ "default", "osc" }) do
    for _, exit in ipairs({ 0, 1 }) do
        test("diff-pair " .. format .. " records exit " .. exit, function()
            local args = { table.unpack(PAIR) }
            for _, arg in ipairs({ "--format", format, "--jobs", "2" }) do
                args[#args + 1] = arg
            end
            local want = exit == 0 and "same" or "differs"
            local out = format == "osc" and "diff.osc" or "diff.txt"
            local r = invoke("diff-pair", args, { { exit = exit, outputs = { [out] = "abc" } } }, want)
            assert(r.outcome == "completed", r.reason)
            assert(r.verdict == want and r.record.diff_exit == exit and r.record.output_bytes == 3)
            assert(r.record.expectation_key == "diff-snapshots.00000000000000000000000000000001+00000000000000000000000000000002")
            assert(r.record.expectation_applied and r.record.expectation_answer == want)
            assert(r.record.meta.to_snapshot_file == "later.pbf")
            local c = r.calls[1]
            assert(c.measured and c.argv[1] == "diff")
            assert(c.argv[2] == DATA .. "base.pbf" and c.argv[3] == DATA .. "later.pbf")
            assert(has(c.argv, "--jobs", "2"))
            assert(has(c.argv, "--suppress-common") == (format == "default"))
            assert(has(c.argv, "--tmp-dir") == (format == "osc"))
            assert(has(c.argv, "--format", "osc") == (format == "osc"))
        end)
    end
end
for _, case in ipairs({
    { exit = 1, trusted = true, want = "same", reason = "declared same" },
    { exit = 0, trusted = true, want = "differs", reason = "declared differs" },
    { exit = 1, trusted = false, reason = "ended with exit 1" },
    { exit = 2, trusted = true, reason = "ended with exit 2" },
}) do
    test("diff-pair rejects " .. case.reason, function()
        local r = invoke("diff-pair", PAIR, { { exit = case.exit, outputs = { ["diff.txt"] = "" } } }, case.want, case.trusted)
        assert(r.outcome == "failed", r.outcome)
        assert(r.reason:find(case.reason, 1, true), r.reason)
    end)
end

for _, indexed in ipairs({ "true", "false" }) do
    test("diff-applied prepares indexed=" .. indexed .. " without measuring setup", function()
        local r = invoke("diff-applied", { "--osc", "9", "--jobs", "2" }, {
            { stdout = indexed .. "\n" }, { outputs = { ["merged.osm.pbf"] = "merged" } },
            { exit = 1, outputs = { ["diff.txt"] = "delta" } },
        }, "differs")
        assert(r.outcome == "completed", r.reason)
        assert(r.verdict == "differs" and r.record.output_bytes == 5)
        assert(r.record.expectation_key == "diff.9" and r.record.expectation_applied)
        assert(r.record.meta.merged_cache == "miss")
        assert((r.record.meta.diff_pair == "raw-vs-indexed") == (indexed == "false"))
        assert(#r.calls == 3 and not r.calls[1].measured and not r.calls[2].measured and r.calls[3].measured)
        local inspect, apply, diff = r.calls[1].argv, r.calls[2].argv, r.calls[3].argv
        assert(inspect[1] == "inspect" and has(inspect, "--get", "indexed"))
        assert(apply[1] == "apply-changes" and apply[2] == DATA .. "base.pbf" and apply[3] == DATA .. "9.osc.gz")
        assert(has(apply, "--force") == (indexed == "false"))
        assert(not has(apply, "--jobs") and not has(apply, "--compression"))
        assert(diff[1] == "diff" and has(diff, "--jobs", "2"))
        for i, arg in ipairs(apply) do
            if arg == "--output" then assert(diff[3] == apply[i + 1]) end
        end
    end)
end

test("diff-applied refuses malformed inspect evidence before applying", function()
    local r = invoke("diff-applied", { "--osc", "9" }, { { stdout = "maybe\n" } })
    assert(r.outcome == "failed" and #r.calls == 1)
    assert(r.reason:find("unexpected answer", 1, true), r.reason)
end)

for _, name in ipairs({ "diff-pair", "diff-applied" }) do
    test(name .. " rejects a requested stop that never fires", function()
        local args = name == "diff-pair" and { table.unpack(PAIR) } or { "--osc", "9" }
        args[#args + 1] = "--stop"; args[#args + 1] = "=DIFF_POINT"
        local calls = name == "diff-pair" and {} or { { stdout = "true" }, {} }
        calls[#calls + 1] = { exit = 0 }
        local r = invoke(name, args, calls)
        assert(r.outcome == "failed", r.outcome)
        assert(r.reason:find("stop_at marker DIFF_POINT", 1, true), r.reason)
    end)
    test(name .. " stops only the measured diff and skips output accounting", function()
        local args = name == "diff-pair" and { table.unpack(PAIR) } or { "--osc", "9" }
        args[#args + 1] = "--stop"; args[#args + 1] = "=DIFF_POINT"
        local calls = name == "diff-pair" and {} or { { stdout = "true" }, {} }
        calls[#calls + 1] = { stopped_at = "DIFF_POINT" }
        local r = invoke(name, args, calls)
        assert(r.outcome == "completed", r.reason)
        assert(r.verdict == "stopped" and r.record.stopped_at == "DIFF_POINT")
        assert(r.record.output_bytes == nil and r.record.diff_exit == nil)
        for i, call in ipairs(r.calls) do
            assert((call.stop_at == "DIFF_POINT") == (i == #r.calls))
        end
    end)
end
