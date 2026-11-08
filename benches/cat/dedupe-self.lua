-- dedupe-self: `cat --dedupe` of a dataset's file with itself, the input
-- either out of the page cache or wholly in it as the timed block starts.
-- Registered in brokkr.d/benches.toml. The typed `brokkr cat --dedupe
-- --bench` runs the same self-merge but leaves the cache state to whatever
-- ran before it, and a cold Japan cell has run 40 % slower than a warm one
-- on identical code, so a comparison across commits needs the state fixed.
--
-- The self-merge puts every blob in a two-blob overlap run: it exercises
-- pass 1's walk and the overlap sweep, never passthrough or the coalesced
-- copy_file_range. Take one iteration per invocation (later iterations of a
-- cold invocation start warm) and alternate the commits under comparison:
--
--   brokkr bench --iterations 1 --commit REF dedupe-self --dataset japan --cache cold
--
-- The preflight reads the input whole (warm). Eviction (cold) happens in the
-- timed block instead, since brokkr hashes the inputs after the preflight
-- and a digest it cannot reuse reads them back in. The timed block then
-- checks residency with fincore before the merge, records it, and fails a
-- cold run that finds any of the input resident or a warm run that finds
-- less than all of it. The iteration wall therefore includes that
-- preparation; compare commits on the pbfhogg child's own wall, which
-- brokkr records beside it (`brokkr results UUID`, the cat child).

local function resident(path)
    local r = binary_with({ program = "fincore" }, "--bytes", "--noheadings", "--output", "RES", path):ok()
    local res = tonumber(r.stdout:match("%d+"))
    if res == nil then fail("fincore gave no resident size for " .. path) end
    return res
end

function preflight(params)
    local path = params.variant.file
    if params.cache == "cold" then
        return
    elseif params.cache == "warm" then
        binary_with({ program = "dd" }, "if=" .. path, "of=/dev/null", "bs=4M", "status=none"):ok()
    else
        fail("--cache takes cold or warm, not " .. params.cache)
    end
end

function bench(params)
    local pbf = params.variant.file
    if params.cache == "cold" then
        binary_with({ program = "dd" }, "if=" .. pbf.path, "iflag=nocache", "count=0", "status=none"):ok()
    end
    local res = resident(pbf.path)
    local size = filesize(pbf)
    record.input_resident_bytes = res
    record.input_bytes = size
    if params.cache == "cold" and res > 0 then
        fail(res .. " bytes of the input are cached in a cold run")
    end
    if params.cache == "warm" and res < size then
        fail("only " .. res .. " of " .. size .. " input bytes are cached in a warm run")
    end
    local out = output("out.osm.pbf")
    binary("cat", pbf, pbf, { dedupe = true, output = out, overwrite = true }):ok()
    record.output_bytes = filesize(out)
    -- --check yes diffs the output against the input with the same build: a
    -- self-merge must reproduce its input's elements exactly. It is a
    -- separate correctness run, not a timing cell: the diff lands in the
    -- iteration wall (the cat child's own wall is unaffected).
    if params.check == "yes" then
        local d = binary("diff", pbf, out)
        record.diff_exit = d.exit
        if d.exit ~= 0 then fail("the self-merge output differs from its input (diff exit " .. tostring(d.exit) .. ")") end
    elseif params.check ~= "no" then
        fail("--check takes yes or no, not " .. params.check)
    end
end
