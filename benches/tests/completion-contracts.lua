-- Benches whose child answers through its exit code complete on either
-- answer and judge neither: no verdict, no answer recorded. Digest form
-- resolution is the other contract here, independent of the mocked input
-- bytes and child implementation.
local FILE = project_root .. "/data/answer.pbf"
local function invoke(name, args, response, trusted, modern)
    local params = { "--dataset", "fixture", "--variant", "indexed" }
    for _, arg in ipairs(args) do params[#params + 1] = arg end
    return run { bench = name, program = "pbfhogg", params = params,
        host = { datasets = { fixture = { pbf = { indexed = { file = "answer.pbf" } } } } },
        files = { [FILE] = "fixture" }, calls = response and { response } or {},
        since = { ["854f9465"] = trusted ~= false, ["16154b0d"] = modern ~= false } }
end
local function has(argv, flag, value)
    for i, arg in ipairs(argv) do
        if arg == flag and (value == nil or argv[i + 1] == value) then return true end
    end
    return false
end
for _, modern in ipairs({ false, true }) do
    for _, form in ipairs({ "auto", "full", "content" }) do
        test("canonical-digest resolves " .. form .. " modern=" .. tostring(modern), function()
            local r = invoke("canonical-digest", { "--form", form, "--direct-io", "yes" },
                { stdout = "digest report", stderr = "summary" }, true, modern)
            assert(r.outcome == "completed", r.reason)
            local resolved = form == "auto" and (modern and "full" or "content") or form
            assert(r.verdict == resolved and r.record.form == resolved)
            assert(r.record.digest == nil and r.record.summary == "summary")
            local argv = r.calls[1].argv
            assert(argv[1] == "digest" and argv[2] == FILE)
            assert(has(argv, "--direct-io"))
            assert(has(argv, "--form") == (modern or resolved == "full"))
            if modern or resolved == "full" then assert(has(argv, "--form", resolved)) end
        end)
    end
end
local checks = {
    { name = "id-order", selector = "--ids", args = { "--full", "yes", "--direct-io", "yes" },
        flags = { "--full", "--direct-io" } },
    { name = "index-integrity", selector = "--index", args = { "--direct-io", "yes" }, flags = { "--direct-io" } },
    { name = "location-integrity", selector = "--locations", args = { "--agreement", "yes" }, flags = { "--agreement" } },
    { name = "ref-integrity", selector = "--refs", args = { "--check-relations", "yes" }, flags = { "--check-relations" } },
}
for _, value in ipairs({ "yes", "no" }) do
    test("ref-integrity refuses undeclared --direct-io " .. value, function()
        local r = invoke("ref-integrity", { "--direct-io", value })
        assert(r.outcome == "refused", r.outcome)
        assert(#r.calls == 0)
    end)
end
test("ref-integrity never forwards --direct-io", function()
    local r = invoke("ref-integrity", {}, { exit = 0 })
    assert(r.outcome == "completed", r.reason)
    assert(not has(r.calls[1].argv, "--direct-io"))
end)
for _, case in ipairs(checks) do
    for _, exit in ipairs({ 0, 1 }) do
        test(case.name .. " completes on exit " .. exit .. " without judging it", function()
            local r = invoke(case.name, case.args, { exit = exit, stderr = "summary" })
            assert(r.outcome == "completed", r.reason)
            assert(r.verdict == nil, tostring(r.verdict))
            assert(r.record.check_exit == nil and r.record.expectation_key == nil)
            assert(r.record.summary == "summary")
            local argv = r.calls[1].argv
            assert(argv[1] == "check" and argv[2] == case.selector and argv[3] == FILE)
            for _, flag in ipairs(case.flags) do assert(has(argv, flag), flag) end
        end)
    end
    test(case.name .. " fails on exit one before the exit split", function()
        local r = invoke(case.name, case.args, { exit = 1 }, false)
        assert(r.outcome == "failed", r.outcome)
    end)
    test(case.name .. " fails on child failure", function()
        local r = invoke(case.name, case.args, { exit = 2, stderr = "failed" })
        assert(r.outcome == "failed", r.outcome)
    end)
end
for _, modern in ipairs({ false, true }) do
    test("canonical-digest content-locations modern=" .. tostring(modern), function()
        local params = { "--dataset", "fixture", "--variant", "indexed", "--form", "content-locations" }
        local r = run { bench = "canonical-digest", program = "pbfhogg", params = params,
            host = { datasets = { fixture = { pbf = { indexed = { file = "answer.pbf" } } } } },
            files = { [FILE] = "fixture" },
            calls = modern and { { stderr = "summary" } } or {},
            since = { ["bc714d0c"] = modern, ["16154b0d"] = true } }
        if modern then
            assert(r.outcome == "completed", r.reason)
            assert(r.verdict == "content-locations")
            assert(has(r.calls[1].argv, "--form", "content-locations"))
        else
            assert(r.outcome == "failed", r.outcome)
            assert(r.reason:find("bc714d0c", 1, true), r.reason)
            assert(#r.calls == 0)
        end
    end)
end
test("canonical-digest fails on child failure", function()
    local r = invoke("canonical-digest", {}, { exit = 2, stderr = "failed" })
    assert(r.outcome == "failed", r.outcome)
end)
-- id-order intentionally has no stop parameter in its registered contract.
for _, name in ipairs({ "canonical-digest", "index-integrity", "location-integrity", "ref-integrity" }) do
    test(name .. " stops with the stopped verdict", function()
        local r = invoke(name, { "--stop", "=ANSWER_POINT" }, { stopped_at = "ANSWER_POINT" })
        assert(r.outcome == "completed", r.reason)
        assert(r.verdict == "stopped" and r.record.stopped_at == "ANSWER_POINT")
        assert(r.calls[1].stop_at == "ANSWER_POINT")
    end)
    test(name .. " fails when the requested stop never fires", function()
        local r = invoke(name, { "--stop", "=ANSWER_POINT" }, { exit = 0 })
        assert(r.outcome == "failed", r.outcome)
        assert(r.reason:find("stop_at marker ANSWER_POINT", 1, true), r.reason)
    end)
end
