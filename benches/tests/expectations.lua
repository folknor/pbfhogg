-- Required expectations must fail before diff preparation or measurement.
local DATA = project_root .. "/data/"
local HOST = { datasets = { denmark = {
    pbf = { indexed = { file = "expect-base.pbf", expect = { diff = { ["4706"] = "differs" } } } },
    osc = { ["4705"] = { file = "expect.osc.gz" } },
    snapshot = { later = { pbf = { indexed = { file = "expect-later.pbf" } } } },
} } }

for _, case in ipairs({
    { name = "diff-pair", args = { "--from", "base", "--to", "later" }, key = "diff-snapshots",
        files = { [DATA .. "expect-base.pbf"] = "", [DATA .. "expect-later.pbf"] = "" } },
    { name = "diff-applied", args = { "--osc", "4705" }, key = "diff.4705",
        files = { [DATA .. "expect-base.pbf"] = "", [DATA .. "expect.osc.gz"] = "" } },
}) do
    test(case.name .. " requires its selected expectation, not a neighboring key", function()
        local params = { "--dataset", "denmark", "--require-expectation", "yes" }
        for _, arg in ipairs(case.args) do table.insert(params, arg) end
        local r = run { bench = case.name, program = "pbfhogg", host = HOST,
            params = params, files = case.files }
        assert(r.outcome == "failed", r.outcome)
        assert(string.find(r.reason, "missing expect." .. case.key, 1, true), r.reason)
        assert(#r.calls == 0, #r.calls)
    end)
end
