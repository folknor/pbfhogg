-- diff-unindexed: text `diff -c` of two snapshots of one dataset, both
-- without indexdata, so the comparison takes the sequential element stream
-- (the block-pair engine needs both inputs indexed). Registered in
-- brokkr.d/benches.toml. The typed `brokkr diff` cannot reach this path: it
-- builds its new side with apply-changes, using --force for an unindexed
-- base, so only the old side remains unindexed. Both typed diff and the
-- general diff-applied bench support --type; typed diff-snapshots does not.
--
-- The diff call is `cold` over both inputs (`brokkr man bench page-cache`):
-- brokkr advises both out after admitting the call and fails the iteration
-- unless none of either is resident before launch. Warm is not offered: a
-- germany pair (two 4.7 GB files) does not stay cached under the bench's
-- memory limit, so a warm state cannot be held. The path runs on one
-- thread well below the drive's read rate, so cold costs little. The call
-- is `measured`, so the result is the diff child's wall, without the
-- preparation. Take one iteration per invocation and alternate the commits
-- under comparison:
--
--   brokkr bench --iterations 1 --commit REF diff-unindexed --dataset germany --new 20260420
--
-- The pair must differ, so exit 0 fails the run as surely as exit 2. The
-- output size and the summary are recorded so arms can be checked for
-- parity.

function preflight(params)
    for _, f in ipairs({ params.old_variant.file, params.new_variant.file }) do
        local why = page_cache(f, "cold")
        if why then fail(why) end
    end
end

-- --type is checked by its choices in brokkr.d/benches.toml.
function validate(params)
    if params.old_variant.file == params.new_variant.file then
        fail("--old and --new name the same file")
    end
end

-- Match the diff dispatch probe, rather than the whole-file `indexed`
-- answer: that answer is false for partially indexed inputs too, including
-- ones whose first blob would select the indexed diff engine. This proves
-- the sequential path; proving zero indexed blobs anywhere needs a new
-- inspect count, since no current public field exposes that count.
local function require_unindexed_front(input)
    local r = binary("inspect", input, { indexed = true, quiet = true })
    if r.exit ~= 1 then
        fail("diff-unindexed requires a first data blob without indexdata; inspect exit "
            .. tostring(r.exit) .. ": " .. r.stderr)
    end
end

function bench(params)
    local old = params.old_variant.file
    local new = params.new_variant.file
    require_unindexed_front(old)
    require_unindexed_front(new)
    local out = output("diff.txt")
    local flags = { ["suppress-common"] = true, output = out, overwrite = true }
    if params.type ~= "all" then flags.type = params.type end
    local d = binary_with({ cold = { old, new }, measured = true }, "diff", old, new, flags)
    record.diff_exit = d.exit
    if d.exit ~= 1 then
        fail("diff exited " .. tostring(d.exit) .. ", expected 1 for a differing pair: " .. d.stderr)
    end
    record.output_bytes = filesize(out)
    record.summary = d.stderr
end
