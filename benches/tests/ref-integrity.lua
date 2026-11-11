-- ref-integrity reads expect.check-refs for a plain run and
-- expect.check-refs-relations under --check-relations yes, never one for the
-- other, and judges the child's exit through benches/lib/expect.lua. The two
-- keys are declared with opposite answers wherever both are present, so a
-- script that read the wrong one fails here.

local FILE = project_root .. "/data/denmark.pbf"
local SPLIT = "854f9465"

local function host(expect)
    return { datasets = { denmark = { pbf = { indexed = { file = "denmark.pbf", expect = expect } } } } }
end

local function relations(flag)
    return { "--dataset", "denmark", "--check-relations", flag }
end

local function check(spec)
    spec.bench = "ref-integrity"
    spec.program = "pbfhogg"
    spec.files = { [FILE] = "" }
    return run(spec)
end

local OPPOSED = { ["check-refs"] = "clean", ["check-refs-relations"] = "violations" }

test("a relations run is judged by check-refs-relations", function()
    local r = check {
        params = relations("yes"), host = host(OPPOSED), since = { [SPLIT] = true },
        calls = { { exit = 1 } },
    }
    assert(r.outcome == "completed", r.reason)
    assert(r.verdict == "violations", tostring(r.verdict))
    assert(r.calls[1].argv[2] == "--refs", r.calls[1].argv[2])
    assert(r.calls[1].argv[4] == "--check-relations", tostring(r.calls[1].argv[4]))
end)

test("a relations run contradicting check-refs-relations fails", function()
    local r = check {
        params = relations("yes"), host = host(OPPOSED), since = { [SPLIT] = true },
        calls = { { exit = 0 } },
    }
    assert(r.outcome == "failed", r.outcome)
    assert(string.find(r.reason, "expect.check-refs-relations declares violations", 1, true), r.reason)
end)

test("a plain run is judged by check-refs", function()
    local r = check { params = relations("no"), host = host(OPPOSED), calls = { { exit = 0 } } }
    assert(r.outcome == "completed", r.reason)
    assert(r.verdict == "clean", tostring(r.verdict))
    assert(r.calls[1].argv[4] == nil, tostring(r.calls[1].argv[4]))
end)

test("a plain run contradicting check-refs fails", function()
    local r = check {
        params = relations("no"), host = host(OPPOSED), since = { [SPLIT] = true },
        calls = { { exit = 1 } },
    }
    assert(r.outcome == "failed", r.outcome)
    assert(string.find(r.reason, "expect.check-refs declares clean", 1, true), r.reason)
end)

test("the opposite declarations fail the opposite runs", function()
    local swapped = { ["check-refs"] = "violations", ["check-refs-relations"] = "clean" }
    local r = check {
        params = relations("yes"), host = host(swapped), since = { [SPLIT] = true },
        calls = { { exit = 1 } },
    }
    assert(r.outcome == "failed", r.outcome)
    assert(string.find(r.reason, "expect.check-refs-relations declares clean", 1, true), r.reason)
    r = check {
        params = relations("no"), host = host(swapped), since = { [SPLIT] = true },
        calls = { { exit = 0 } },
    }
    assert(r.outcome == "failed", r.outcome)
    assert(string.find(r.reason, "expect.check-refs declares violations", 1, true), r.reason)
end)

test("an absent relations declaration accepts either answer, whatever check-refs says", function()
    for _, plain in ipairs({ "clean", "violations" }) do
        for _, exit in ipairs({ 0, 1 }) do
            local r = check {
                params = relations("yes"), host = host({ ["check-refs"] = plain }),
                since = { [SPLIT] = true }, calls = { { exit = exit } },
            }
            assert(r.outcome == "completed", plain .. " exit " .. exit .. ": " .. tostring(r.reason))
        end
    end
end)

test("a declared violations answer is refused before the exit split", function()
    local r = check {
        params = relations("yes"), host = host(OPPOSED), since = { [SPLIT] = false },
    }
    assert(r.stage == "preflight", r.stage)
    assert(r.outcome == "refused", r.outcome)
    assert(string.find(r.reason, "expect.check-refs-relations declares violations", 1, true), r.reason)
end)

test("an undeclared exit 1 before the exit split fails", function()
    local r = check {
        params = relations("no"), host = host(nil), since = { [SPLIT] = false },
        calls = { { exit = 1 } },
    }
    assert(r.outcome == "failed", r.outcome)
    assert(string.find(r.reason, "ended with exit 1", 1, true), r.reason)
end)
