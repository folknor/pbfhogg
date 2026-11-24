-- Synthetic workload cells must preserve child evidence and stop semantics.
local DATA = project_root .. "/data/"
local OSC = "elapsed_ms=7.osc.gz"
local DIAGNOSTIC = "Reading OSC: " .. DATA .. OSC .. "\n"
local function invoke(name, args, calls)
    local params = { "--dataset", "fixture" }
    for _, arg in ipairs(args) do params[#params + 1] = arg end
    local files = { [DATA .. "base.pbf"] = "" }
    if name == "merge-workload" then files[DATA .. OSC] = "" end
    return run { bench = name, program = "pbfhogg-bench", params = params,
        host = { datasets = { fixture = {
            pbf = { indexed = { file = "base.pbf" } }, osc = { ["9"] = { file = OSC } },
        } } }, files = files, calls = calls }
end
local function has(argv, key, value)
    for i, arg in ipairs(argv) do
        if arg == key and (value == nil or argv[i + 1] == value) then return true end
    end
    return false
end
local cases = {
    { name = "merge-workload", command = "merge", args = { "--osc-seq", "9", "--compressions", "zlib" },
        flags = { { "--io-mode", "buffered" }, { "--compression", "zlib" }, { "--staging-dir" } }, out = "buffered.osm.pbf" },
}
for _, case in ipairs(cases) do
    test(case.name .. " retains elapsed evidence and argv", function()
        local outputs = {}
        if case.out then outputs[case.out] = "abc" end
        local r = invoke(case.name, case.args, { { stdout = "elapsed_ms=17\n", stderr = DIAGNOSTIC, outputs = outputs } })
        assert(r.outcome == "completed", r.reason)
        assert(#r.calls == 1 and r.calls[1].measured)
        local argv = r.calls[1].argv
        assert(argv[1] == case.command and argv[2] == DATA .. "base.pbf")
        for _, flag in ipairs(case.flags) do assert(has(argv, table.unpack(flag)), table.concat(argv, " ")) end
        local cell = case.out and r.record.cells[1] or r.record.child
        assert(cell.elapsed_ms == 17 and cell.summary == DIAGNOSTIC)
        if case.out then
            assert(argv[3] == DATA .. OSC)
            assert(cell.io == "buffered" and cell.output_bytes == 3)
        end
    end)
    test(case.name .. " rejects completion before the requested stop", function()
        local args = { table.unpack(case.args) }
        args[#args + 1] = "--stop"; args[#args + 1] = "=WORK_POINT"
        local r = invoke(case.name, args, { { exit = 0 } })
        assert(r.outcome == "failed", r.outcome)
        assert(r.reason:find("stop_at marker WORK_POINT", 1, true), r.reason)
    end)
    test(case.name .. " rejects missing child timing", function()
        local r = invoke(case.name, case.args, { { stderr = DIAGNOSTIC } })
        assert(r.outcome == "failed", r.outcome)
        assert(r.reason:find("did not report a single elapsed_ms line on stdout", 1, true), r.reason)
    end)
    test(case.name .. " rejects malformed or repeated child timing", function()
        for _, stdout in ipairs({ "elapsed_ms=17junk\n", "elapsed_ms=17\nelapsed_ms=23\n" }) do
            local r = invoke(case.name, case.args, { { stdout = stdout } })
            assert(r.outcome == "failed", r.outcome)
            assert(r.reason:find("did not report a single elapsed_ms line on stdout", 1, true), r.reason)
        end
    end)
    test(case.name .. " rejects failed child even with timing", function()
        local r = invoke(case.name, case.args, { { exit = 2, stdout = "elapsed_ms=17\n" } })
        assert(r.outcome == "failed", r.outcome)
    end)
    test(case.name .. " accepts stopped child without timing or output", function()
        local args = { table.unpack(case.args) }
        args[#args + 1] = "--stop"; args[#args + 1] = "=WORK_POINT"
        local r = invoke(case.name, args, { { stopped_at = "WORK_POINT" } })
        assert(r.outcome == "completed", r.reason)
        assert(r.verdict == "stopped" and r.calls[1].stop_at == "WORK_POINT")
        local cell = case.out and r.record.cells[1] or r.record.child
        assert(cell.stopped_at == "WORK_POINT" and cell.elapsed_ms == nil and cell.output_bytes == nil)
    end)
    test(case.name .. " refuses an undeclared direct I/O param before children", function()
        local args = { table.unpack(case.args) }
        args[#args + 1] = "--direct-io"; args[#args + 1] = "no"
        local r = invoke(case.name, args, {})
        assert(r.outcome == "refused" and #r.calls == 0)
    end)
end

test("merge-workload records both backends but measures buffered only", function()
    local r = invoke("merge-workload", { "--osc-seq", "9", "--compressions", "none", "--uring", "yes" }, {
        { stdout = "elapsed_ms=17\n", outputs = { ["buffered.osm.pbf"] = "abc" } },
        { stdout = "elapsed_ms=23\n", outputs = { ["uring.osm.pbf"] = "abcd" } },
    })
    assert(r.outcome == "completed", r.reason)
    assert(#r.calls == 2 and r.calls[1].measured and not r.calls[2].measured)
    assert(has(r.calls[1].argv, "--io-mode", "buffered") and has(r.calls[2].argv, "--io-mode", "uring"))
    assert(#r.record.cells == 2)
    assert(r.record.cells[1].io == "buffered" and r.record.cells[1].elapsed_ms == 17 and r.record.cells[1].output_bytes == 3)
    assert(r.record.cells[2].io == "uring" and r.record.cells[2].elapsed_ms == 23 and r.record.cells[2].output_bytes == 4)
end)
