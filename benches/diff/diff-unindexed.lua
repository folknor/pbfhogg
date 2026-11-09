-- diff-unindexed: text `diff -c` of two snapshots of one dataset, both
-- without indexdata, so the comparison takes the sequential element stream
-- (the block-pair engine needs both inputs indexed). Registered in
-- brokkr.d/benches.toml. The typed `brokkr diff` cannot reach this path: it
-- builds its new side with `apply-changes` on the chosen variant, which
-- refuses an unindexed base without apply-changes' own `--force`, and it
-- has no `--type`. `brokkr diff-snapshots` has no `--type` either.
--
-- Both inputs are evicted from the page cache in the timed block and
-- fincore checks that none of either is resident before the diff starts.
-- Warm is not offered: a germany pair (two 4.7 GB files) does not stay
-- cached under the bench's memory limit, so a warm state cannot be held.
-- The path runs on one thread well below the drive's read rate, so cold
-- costs little. Take one iteration per invocation, alternate the commits
-- under comparison, and compare the diff child's own wall
-- (`brokkr results UUID`), not the iteration wall:
--
--   brokkr bench --iterations 1 --commit REF diff-unindexed --dataset germany --new 20260420
--
-- The pair must differ, so exit 0 fails the run as surely as exit 2. The
-- output size and the summary are recorded so arms can be checked for
-- parity.

local cache = require("cache")

-- --type is checked by its choices in brokkr.d/benches.toml.
function validate(params)
    if params.old_variant.file == params.new_variant.file then
        fail("--old and --new name the same file")
    end
end

function bench(params)
    local old = params.old_variant.file
    local new = params.new_variant.file
    for _, f in ipairs({ old, new }) do
        binary_with({ program = "dd" }, "if=" .. f.path, "iflag=nocache", "count=0", "status=none"):ok()
    end
    for _, f in ipairs({ old, new }) do
        local res = cache.resident(f.path)
        if res > 0 then
            fail(res .. " bytes of " .. f.path .. " are cached after eviction")
        end
    end
    local out = output("diff.txt")
    local flags = { ["suppress-common"] = true, output = out, overwrite = true }
    if params.type ~= "all" then flags.type = params.type end
    local d = binary("diff", old, new, flags)
    record.diff_exit = d.exit
    if d.exit ~= 1 then
        fail("diff exited " .. tostring(d.exit) .. ", expected 1 for a differing pair: " .. d.stderr)
    end
    record.output_bytes = filesize(out)
    record.summary = d.stderr
end
