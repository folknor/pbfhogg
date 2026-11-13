-- Measurement contracts only. Worker artefacts are deliberately absent.
local DATA = project_root .. "/data/"
local ROOT = "/brokkr-contract/iteration/"
local function invoke(scenario, worker, snapshot)
    local params = { "--scenario", "sample", "--dataset", "fixture", "--variant", "raw" }
    if snapshot then params[#params + 1] = "--snapshot"; params[#params + 1] = "old" end
    local pbf = { raw = { file = "raw.pbf" }, indexed = { file = "indexed.pbf" } }
    local dataset = { pbf = pbf, snapshot = { old = { pbf = {
        raw = { file = "old-raw.pbf" }, indexed = { file = "old-indexed.pbf" },
    } } } }
    local calls = { { stdout = json.encode({ protocol = 1, scenarios = { scenario } }) } }
    if worker then calls[#calls + 1] = worker end
    return run { bench = "library-scenario", params = params,
        host = { datasets = { fixture = dataset } }, calls = calls }
end
local function scenario(roles)
    return { id = "sample", deadline_s = 37, roles = roles }
end
local function has(list, value)
    for _, v in ipairs(list) do if v == value then return true end end
    return false
end
local function measurement(r, hotpath)
    assert(r.outcome == "completed", r.reason)
    assert(#r.calls == 2)
    local discovery, worker = r.calls[1], r.calls[2]
    assert(discovery.argv[1] == "--describe-harness" and #discovery.argv == 1)
    assert(discovery.deadline == 60 and not discovery.measured and discovery.hotpath == nil)
    assert(worker.measured and worker.deadline == 37 and worker.hotpath == hotpath)
    assert(#worker.argv == 6)
    assert(worker.argv[1] == "--test-harness" and worker.argv[2] == ROOT .. "out/request.json")
    assert(worker.argv[3] == "--private-dir" and worker.argv[4] == ROOT .. "scratch/private")
    assert(worker.argv[5] == "--artefact-dir" and worker.argv[6] == ROOT .. "scratch/artefacts")
    assert(has(worker.reads, worker.argv[2]) and not has(worker.writes, worker.argv[2]))
    assert(has(worker.writes, worker.argv[4]) and has(worker.writes, worker.argv[6]))
    assert(r.verdict == nil)
    for key, _ in pairs(r.record) do assert(false, "unexpected measurement record field " .. key) end
    return worker
end

test("library measures one direct worker with declared variants, not --variant", function()
    local r = invoke(scenario({
        { id = "input", source = "dataset", kind = "pbf", variant = "indexed" },
        { id = "raw", source = "dataset", kind = "pbf", variant = "raw" },
        { id = "out", source = "output", kind = "pbf" },
    }), {})
    local worker = measurement(r)
    assert(#worker.reads == 3 and #worker.writes == 3)
    assert(has(worker.reads, DATA .. "indexed.pbf") and has(worker.reads, DATA .. "raw.pbf"))
    assert(has(worker.writes, ROOT .. "out/role-out"))
end)

test("library selects both variants from the named snapshot", function()
    local worker = measurement(invoke(scenario({
        { id = "input", source = "dataset", kind = "pbf", variant = "indexed" },
        { id = "raw", source = "dataset", kind = "pbf", variant = "raw" },
    }), {}, true))
    assert(has(worker.reads, DATA .. "old-indexed.pbf") and has(worker.reads, DATA .. "old-raw.pbf"))
    assert(not has(worker.reads, DATA .. "indexed.pbf"))
end)

test("library declares generated files, seeded outputs and directory writes", function()
    local worker = measurement(invoke(scenario({
        { id = "generated", source = "generated", kind = "file" },
        { id = "out", source = "output", kind = "pbf", seeded = true },
        { id = "dir", source = "generated", kind = "directory" },
    }), {}))
    assert(#worker.writes == 5 and #worker.reads == 1)
    assert(has(worker.writes, ROOT .. "out/role-generated"))
    assert(has(worker.writes, ROOT .. "out/role-out"))
    assert(has(worker.writes, ROOT .. "scratch/role-dir"))
end)

test("library marks only a declared hotpath worker", function()
    local s = scenario({}); s.hotpath = true
    measurement(invoke(s, { hotpath = { format = "hotpath-v1", functions = {} } }), "worker")
end)

for _, role in ipairs({
    { id = "bad", source = "mystery", kind = "pbf" },
    { id = "bad", source = "dataset", kind = "file" },
    { id = "bad", source = "output", kind = "mystery" },
    { id = "bad", source = "generated", kind = "pbf", seeded = true },
    { id = "bad", source = "dataset", kind = "pbf", variant = "missing" },
    { id = "bad", source = "output", kind = "directory", seeded = true },
}) do
    test("library refuses unsupported role " .. json.encode(role), function()
        local r = invoke(scenario({ role }))
        assert(r.outcome == "failed" and #r.calls == 1, r.reason)
    end)
end

test("library refuses unknown scenarios before launching a worker", function()
    local s = scenario({}); s.id = "other"
    local r = invoke(s)
    assert(r.outcome == "failed" and r.reason:find("unknown scenario sample", 1, true), r.reason)
end)

test("library uses ordinary completion failure without judging artefacts", function()
    local r = invoke(scenario({}), { exit = 2, stderr = "worker failed" })
    assert(r.outcome == "failed" and #r.calls == 2, r.reason)
    assert(r.reason:find("worker failed", 1, true), r.reason)
end)

