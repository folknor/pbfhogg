-- Digest compatibility and integrity declarations are verdict contracts,
-- independent of the mocked input bytes and child implementation.
local FILE = project_root .. "/data/verdict.pbf"
local function invoke(name, args, response, declarations, trusted, modern)
    local params = { "--dataset", "fixture", "--variant", "indexed" }
    for _, arg in ipairs(args) do params[#params + 1] = arg end
    return run { bench = name, program = "pbfhogg", params = params,
        host = { datasets = { fixture = { pbf = { indexed = {
            file = "verdict.pbf", expect = declarations,
        } } } } }, files = { [FILE] = "fixture" }, calls = response and { response } or {},
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
                { stdout = "digest report", stderr = "summary" }, nil, true, modern)
            assert(r.outcome == "completed", r.reason)
            local resolved = form == "auto" and (modern and "full" or "content") or form
            assert(r.verdict == resolved and r.record.form == resolved)
            assert(r.record.digest == "digest report" and r.record.summary == "summary")
            local argv = r.calls[1].argv
            assert(argv[1] == "digest" and argv[2] == FILE)
            assert(has(argv, "--direct-io"))
            assert(has(argv, "--form") == (modern or resolved == "full"))
            if modern or resolved == "full" then assert(has(argv, "--form", resolved)) end
        end)
    end
end
local checks = {
    { name = "id-order", selector = "--ids", args = { "--full", "yes", "--direct-io", "yes" }, key = "check-ids" },
    { name = "location-integrity", selector = "--locations", args = { "--agreement", "no" }, key = "check-locations" },
    { name = "location-integrity", selector = "--locations", args = { "--agreement", "yes" }, key = "check-locations-agreement" },
}
for _, case in ipairs(checks) do
    for _, exit in ipairs({ 0, 1 }) do
        local want = exit == 0 and "clean" or "violations"
        test(case.key .. " accepts declared " .. want, function()
            local declarations = { ["check-locations"] = "clean", ["check-locations-agreement"] = "violations" }
            declarations[case.key] = want
            local r = invoke(case.name, case.args, { exit = exit, stderr = "summary" }, declarations)
            assert(r.outcome == "completed", r.reason)
            assert(r.verdict == want and r.record.check_exit == exit)
            assert(r.record.expectation_key == case.key and r.record.expectation_applied)
            assert(r.record.summary == "summary")
            local argv = r.calls[1].argv
            assert(argv[1] == "check" and argv[2] == case.selector and argv[3] == FILE)
            if case.name == "id-order" then
                assert(has(argv, "--full") and has(argv, "--direct-io"))
            else
                assert(has(argv, "--agreement") == (case.key == "check-locations-agreement"))
            end
        end)
        test(case.key .. " rejects contradiction of " .. want, function()
            local r = invoke(case.name, case.args, { exit = 1 - exit }, { [case.key] = want })
            assert(r.outcome == "failed", r.outcome)
            assert(r.reason:find("expect." .. case.key, 1, true), r.reason)
        end)
    end
    test(case.key .. " refuses untrusted violations before measurement", function()
        local r = invoke(case.name, case.args, nil, { [case.key] = "violations" }, false)
        assert(r.outcome == "refused", r.outcome)
        assert(#r.calls == 0)
        assert(r.reason:find("cannot be trusted", 1, true), r.reason)
    end)
    test(case.key .. " rejects untrusted exit one", function()
        local r = invoke(case.name, case.args, { exit = 1 }, nil, false)
        assert(r.outcome == "failed", r.outcome)
    end)
end
for _, name in ipairs({ "canonical-digest", "id-order", "location-integrity" }) do
    test(name .. " rejects child failure", function()
        local r = invoke(name, {}, { exit = 2, stderr = "failed" })
        assert(r.outcome == "failed", r.outcome)
    end)
    -- id-order intentionally has no stop parameter in its registered contract.
    if name ~= "id-order" then
        test(name .. " stops without claiming a complete verdict", function()
            local r = invoke(name, { "--stop", "=VERDICT_POINT" }, { stopped_at = "VERDICT_POINT" })
            assert(r.outcome == "completed", r.reason)
            assert(r.verdict == "stopped" and r.record.stopped_at == "VERDICT_POINT")
            assert(r.record.check_exit == nil)
            assert(r.calls[1].stop_at == "VERDICT_POINT")
        end)
        test(name .. " fails when the requested stop never fires", function()
            local r = invoke(name, { "--stop", "=VERDICT_POINT" }, { exit = 0 })
            assert(r.outcome == "failed", r.outcome)
            assert(r.reason:find("stop_at marker VERDICT_POINT", 1, true), r.reason)
        end)
    end
end
