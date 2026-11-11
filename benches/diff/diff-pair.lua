-- This snapshot workload does not exercise verbose sequence budgeting.
-- A separate timing cell needs synthetic modified-way PBF pairs with 30,000
-- refs: disjoint ranges and every 60th ref changed. Preparation must precede
-- the measured diff --verbose child, whose exit must be 1 and whose output
-- must distinguish replacement from itemized edits. No existing command or
-- bench preparation helper builds those pairs; registering a snapshot cell
-- here would not establish that contract. Supply an untimed fixture producer
-- before adding that timing workload.
--
-- diff-pair: typed diff-snapshots, two independently resolved inputs, with
-- no merge or cache preparation. Text suppresses common lines; OSC uses
-- iteration scratch. Same-file pairs are allowed and answer same.
-- Both variant params must match; supply both for non-indexed input because
-- param defaults cannot depend on another param. Binding both files keeps
-- their reservations, digests and pairing in brokkr's input machinery.
-- measured = true ranks iterations by the diff child. Profiling and stops
-- apply only to it. Outputs live under host scratch's bench-iterations.
-- Existing diff-text and diff-unindexed retain their fixed cache workloads.
--
--   brokkr bench --dry-run diff-pair --from base --to twin --format default,osc
--   brokkr bench --hotpath diff-pair --from base --to 20260411
local diff = require("diff")
local expect = require("expect")

function validate(params, keys)
    diff.validate(params)
    if keys.variant ~= keys["to-variant"] then fail("--variant and --to-variant must match") end
end

function bench(params)
    local old = params.variant
    local new = params["to-variant"]
    record.meta = { to_snapshot_file = new.file.path:match("[^/]+$"),
        to_snapshot_file_mb = filesize(new.file) / 1000000 }
    local want
    local key = "diff-snapshots"
    if old.xxhash and new.xxhash then
        local a, b = old.xxhash, new.xxhash
        if a > b then a, b = b, a end
        local declared = params.dataset.expect
        want = declared and declared["diff-snapshots"]
        want = want and want[a .. "+" .. b]
        key = key .. "." .. a .. "+" .. b
    end
    want = expect.selected(params, key, want)
    return diff.run(params, old.file, new.file, want)
end
