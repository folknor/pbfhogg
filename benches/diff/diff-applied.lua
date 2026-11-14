-- diff-applied: typed diff's apply-changes preparation then base vs merged
-- comparison. Only the diff child is marked for hotpath/alloc and stops;
-- measured = true ranks iterations by that child, excluding preparation.
-- Merge uses default compression, jobs and I/O, as the typed runner does.
-- Scan every blob for indexdata; raw or partially indexed bases get --force and
-- record.meta.diff_pair = raw-vs-indexed. Text always suppresses common
-- lines. For fixed warm/cold text paths use diff-text or diff-unindexed.
-- Files live under host scratch's bench-iterations and are reclaimed after
-- each iteration, so the merged output is never reused (merged_cache miss).
-- --osc is typed --osc-seq; omitted binds the sole scoped entry via
-- default_only, refusing zero or multiple entries. No base fallback.
--
--   brokkr bench --dry-run diff-applied --osc 4705 --format default,osc
--   brokkr bench --alloc diff-applied --variant raw --type way
local diff = require("diff")
local expect = require("expect")

function validate(params)
    diff.validate(params)
end

function bench(params, keys)
    local want = params.variant.expect and params.variant.expect.diff
    want = expect.selected(params, "diff." .. keys.osc, want and want[keys.osc])
    local raw = diff.unindexed(params.variant.file)
    record.meta = { merged_cache = "miss" }
    if raw then record.meta.diff_pair = "raw-vs-indexed" end
    local merged = output("merged.osm.pbf")
    binary("apply-changes", params.variant.file, params.osc.file, {
        output = merged, overwrite = true, force = raw,
    }):ok()
    return diff.run(params, params.variant.file, merged, want)
end
