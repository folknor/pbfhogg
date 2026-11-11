-- sort-cold: `sort` of a dataset's file into scratch, the input out of the
-- page cache as the sort starts. Registered in brokkr.d/benches.toml. Use
-- `--force yes` for an unindexed input (`pbf.raw`), equivalent to typed
-- `brokkr sort --force-sort`. sort-pbf covers the typed workload without
-- cache preparation.
--
-- An unindexed input sends every blob through pass 1's payload scan (pread,
-- decompress, ID and tag-key scan, on pass 1's worker pool), so pass 1 is judged by its SORT_INDEX_BUILD span
-- (`brokkr sidecar UUID --durations`) and peak anon (`--stat anon`). Take one
-- iteration per invocation and alternate the commits under comparison:
--
--   brokkr bench --iterations 1 --commit REF sort-cold --dataset europe --variant raw --force yes
--
-- The sort call is `cold` over its input (`brokkr man bench page-cache`):
-- brokkr advises the file out after admitting the call and fails the
-- iteration unless none of it is resident before launch, storing the
-- account with the child. The call is `measured`, so the result is the
-- sort child's wall, without that preparation. The preflight checks only
-- what page_cache() can see beforehand. --force is checked by its choices
-- in brokkr.d/benches.toml.

function preflight(params)
    local why = page_cache(params.variant.file, "cold")
    if why then fail(why) end
end

function bench(params)
    local pbf = params.variant.file
    record.input_bytes = filesize(pbf)
    local out = output("out.osm.pbf")
    binary_with({ cold = { pbf }, measured = true },
        "sort", pbf, { force = params.force == "yes", output = out, overwrite = true }):ok()
    record.output_bytes = filesize(out)
end
