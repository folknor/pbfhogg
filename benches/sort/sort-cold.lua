-- sort-cold: `sort` of a dataset's file into scratch, the input evicted from
-- the page cache as the timed block starts. Registered in
-- brokkr.d/benches.toml. Use `--force yes` for an unindexed input
-- (`pbf.raw`), equivalent to typed `brokkr sort --force-sort`.
-- sort-pbf covers the typed workload without cache preparation.
--
-- An unindexed input sends every blob through pass 1's payload scan (pread,
-- decompress, ID and tag-key scan, on pass 1's worker pool), so pass 1 is judged by its SORT_INDEX_BUILD span
-- (`brokkr sidecar UUID --durations`) and peak anon (`--stat anon`). Take one
-- iteration per invocation and alternate the commits under comparison:
--
--   brokkr bench --iterations 1 --commit REF sort-cold --dataset europe --variant raw --force yes
--
-- Eviction happens in the timed block, after brokkr hashes the inputs, and
-- fincore checks it before the sort; the iteration wall therefore includes
-- that preparation, so compare commits on the sort child's own wall
-- (`brokkr results UUID`). --force is checked by its choices in
-- brokkr.d/benches.toml.

local function resident(path)
    local r = binary_with({ program = "fincore" }, "--bytes", "--noheadings", "--output", "RES", path):ok()
    local res = tonumber(r.stdout:match("%d+"))
    if res == nil then fail("fincore gave no resident size for " .. path) end
    return res
end

function bench(params)
    local pbf = params.variant.file
    binary_with({ program = "dd" }, "if=" .. pbf.path, "iflag=nocache", "count=0", "status=none"):ok()
    local res = resident(pbf.path)
    record.input_resident_bytes = res
    record.input_bytes = filesize(pbf)
    if res > 0 then
        fail(res .. " bytes of the input are cached after eviction")
    end
    local out = output("out.osm.pbf")
    binary("sort", pbf, { force = params.force == "yes", output = out, overwrite = true }):ok()
    record.output_bytes = filesize(out)
end
