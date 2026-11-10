-- diff-applied: typed diff's apply-changes preparation then base vs merged
-- comparison. Only the diff child is marked for hotpath/alloc and stops;
-- measured = true ranks iterations by that child, excluding preparation.
-- Merge uses default compression, jobs and I/O, as the typed runner does.
-- Scan every blob for indexdata; raw or partially indexed bases get --force and
-- record.meta.diff_pair = raw-vs-indexed. Text always suppresses common
-- lines. For fixed warm/cold text paths use diff-text or diff-unindexed.
-- Files live under host scratch's bench-iterations and are reclaimed after
-- each iteration. Cache reuse is refused until persistent files exist.
-- --osc is typed --osc-seq; omitted binds the sole scoped entry via
-- default_only, refusing zero or multiple entries. No base fallback.
--
--   brokkr bench --dry-run diff-applied --osc 4705 --format default,osc
--   brokkr bench --alloc diff-applied --variant raw --type way
local diff = require("diff")

function validate(params)
    diff.validate(params)
    if params["keep-cache"] == "yes" then
        fail("--keep-cache yes needs persistent merged outputs; iteration files are reclaimed")
    end
    if params["io-uring"] == "yes" or params.compression then
        fail("diff accepts neither io_uring nor compression; merge preparation uses defaults")
    end
end

function bench(params, keys)
    local raw = diff.unindexed(params.variant.file)
    record.meta = { merged_cache = "miss" }
    if raw then record.meta.diff_pair = "raw-vs-indexed" end
    local merged = output("merged.osm.pbf")
    binary("apply-changes", params.variant.file, params.osc.file, {
        output = merged, overwrite = true, force = raw,
    }):ok()
    local want = params.variant.expect and params.variant.expect.diff
    want = want and want[keys.osc]
    return diff.run(params, params.variant.file, merged, want)
end
