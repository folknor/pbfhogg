-- dedupe-self: `cat --dedupe` of a dataset's file with itself, the input
-- either out of the page cache or wholly in it as the merge starts.
-- Registered in brokkr.d/benches.toml. The typed `brokkr cat --dedupe
-- --bench` runs the same self-merge but leaves the cache state to whatever
-- ran before it, and a cold Japan cell has run 40 % slower than a warm one
-- on identical code, so a comparison across commits needs the state fixed.
--
-- The self-merge puts every blob in a two-blob overlap run: it exercises
-- pass 1's walk and the overlap sweep, never passthrough or the coalesced
-- copy_file_range (dedupe-pair reaches those). Take one iteration per
-- invocation and alternate the commits under comparison:
--
--   brokkr bench --iterations 1 --commit REF dedupe-self --dataset japan --cache cold
--
-- The cat call is `cold` or `warm` over its input by --cache (`brokkr man
-- bench page-cache`): brokkr advises the file out or reads it through after
-- admitting the call, and fails the iteration unless none of it (cold) or
-- all of it (warm) is resident before launch, storing the account with the
-- child. The call is `measured`, so the result is the cat child's wall,
-- without that preparation. The preflight checks only what page_cache()
-- can see beforehand. --cache and --check are checked by their choices in
-- brokkr.d/benches.toml.

function preflight(params)
    local why = page_cache(params.variant.file, params.cache)
    if why then fail(why) end
end

function bench(params)
    local pbf = params.variant.file
    record.input_bytes = filesize(pbf)
    local out = output("out.osm.pbf")
    binary_with({ [params.cache] = { pbf }, measured = true },
        "cat", pbf, pbf, { dedupe = true, output = out, overwrite = true }):ok()
    record.output_bytes = filesize(out)
    -- --check yes diffs the output against the input with the same build: a
    -- self-merge must reproduce its input's elements exactly. It is a
    -- separate correctness run, not a timing cell: the diff is not
    -- measured, so the result stays the cat child's wall.
    if params.check == "yes" then
        local d = binary("diff", pbf, out)
        record.diff_exit = d.exit
        if d.exit ~= 0 then fail("the self-merge output differs from its input (diff exit " .. tostring(d.exit) .. ")") end
    end
end
