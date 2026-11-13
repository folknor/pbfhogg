-- The argv library-scenario builds: one measured, marked worker launch.
local DATA = project_root .. "/data/"
local ROOT = "/brokkr-contract/iteration/"

test("library-scenario launches one bench-mode worker over every bound variant", function()
    local r = run { bench = "library-scenario",
        params = { "--scenario", "sample", "--dataset", "fixture", "--inputs", "indexed,raw" },
        host = { datasets = { fixture = { pbf = {
            raw = { file = "raw.pbf" }, indexed = { file = "indexed.pbf" },
        } } } },
        calls = { {} } }
    assert(r.outcome == "completed", r.reason)
    assert(#r.calls == 1)
    local worker = r.calls[1]
    assert(worker.measured and worker.hotpath == "worker" and worker.deadline == nil)
    local want = { "--bench-harness", "sample", "--work", ROOT .. "scratch/work",
        "--dataset", "indexed", DATA .. "indexed.pbf", "--dataset", "raw", DATA .. "raw.pbf" }
    assert(#worker.argv == #want, #worker.argv)
    for i, word in ipairs(want) do assert(worker.argv[i] == word, worker.argv[i]) end
end)
