-- dedupe-pair: `cat --dedupe` of two different snapshots of one dataset,
-- both inputs either out of the page cache or wholly in it as the timed
-- block starts. Registered in brokkr.d/benches.toml. It is dedupe-self with
-- a second input: a self-merge puts every blob in an overlap run and never
-- reaches passthrough, while a pair whose ID ranges are disjoint over long
-- stretches sends those stretches through passthrough, and on Linux
-- through the coalesced copy_file_range.
--
-- Two denmark shapes it was written for:
--
--   --with one-relation  base against a file holding only base's
--     highest-id relation (getid of it). One overlap run of two relation
--     blobs; every other base frame passes through in one copy stretch,
--     so the cat wall is pass 1 plus passthrough: the leg itself, end to
--     end.
--   --with ways-only     base against its ways-only snapshot. Every way
--     blob overlaps and base's nodes and relations pass through, so the
--     wall mixes passthrough with the overlap sweep and its transitions.
--
-- The cat child's counters say what the plan did; read them before
-- trusting a cell to measure passthrough:
--
--   brokkr sidecar UUID --child N --counters --grep cat_dedupe
--
-- cat_dedupe_blobs_passthrough counts the frames passed through, and
-- cat_dedupe_copy_range_calls and cat_dedupe_copy_range_coalesced split
-- the copy route into stretches started and frames folded into one. Take
-- one iteration per invocation and alternate the commits under comparison:
--
--   brokkr bench --iterations 1 --commit REF dedupe-pair --with one-relation --cache cold
--
-- Cache handling is dedupe-self's: the timed block reads both inputs whole
-- (warm) or evicts both (cold), and fincore checks both before the merge.
-- Compare commits on the cat child's own wall. --cache and --check are
-- checked by their choices in brokkr.d/benches.toml.

local function resident(path)
    local r = binary_with({ program = "fincore" }, "--bytes", "--noheadings", "--output", "RES", path):ok()
    local res = tonumber(r.stdout:match("%d+"))
    if res == nil then fail("fincore gave no resident size for " .. path) end
    return res
end

function validate(params)
    -- validate() sees each file as its path string; bench sees a handle.
    if params.base_variant.file == params.with_variant.file then
        fail("--base and --with name the same file; dedupe-self is the self-merge")
    end
end

function bench(params)
    local base = params.base_variant.file
    local with = params.with_variant.file
    for _, f in ipairs({ base, with }) do
        if params.cache == "cold" then
            binary_with({ program = "dd" }, "if=" .. f.path, "iflag=nocache", "count=0", "status=none"):ok()
        else
            binary_with({ program = "dd" }, "if=" .. f.path, "of=/dev/null", "bs=4M", "status=none"):ok()
        end
    end
    for name, f in pairs({ base = base, with = with }) do
        local res = resident(f.path)
        local size = filesize(f)
        record[name .. "_resident_bytes"] = res
        record[name .. "_bytes"] = size
        if params.cache == "cold" and res > 0 then
            fail(res .. " bytes of " .. f.path .. " are cached in a cold run")
        end
        if params.cache == "warm" and res < size then
            fail("only " .. res .. " of " .. size .. " bytes of " .. f.path .. " are cached in a warm run")
        end
    end
    local out = output("out.osm.pbf")
    binary("cat", base, with, { dedupe = true, output = out, overwrite = true }):ok()
    record.output_bytes = filesize(out)
    -- --check yes diffs the output against the base with the same build.
    -- For a second input whose elements are a subset of the base's (both
    -- shapes above) the merge must have the base's element content; diff
    -- exit 0 says that much, not that metadata or encoding match. A
    -- correctness run, not a timing cell.
    if params.check == "yes" then
        local d = binary("diff", base, out)
        record.diff_exit = d.exit
        if d.exit ~= 0 then fail("the merge output differs from the base (diff exit " .. tostring(d.exit) .. ")") end
    end
end
