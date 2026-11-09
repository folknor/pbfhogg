-- diff-text: text diff of two indexed snapshots of one dataset through the
-- block-pair work units, printing common lines (no -c) and optionally -v
-- details, or with --common no for diff -c. Registered in
-- brokkr.d/benches.toml. The typed `brokkr diff` and `brokkr diff-snapshots`
-- text cells always pass --suppress-common and never -v, so they cannot
-- reach the common-line element cut or verbose details; this can.
--
-- Shapes it has run: denmark base against the 320k-per-blob repack (every
-- element a common line; the one-sided oversized frames are clipped at the
-- other side's cuts, so units stay within their channel), and germany base
-- against 20260420 with -c -v (real verbose details). Both inputs are read whole into
-- the page cache before the timed block and checked resident with fincore
-- there, so the cell times formatting and scheduling, not the drive. Take
-- one iteration per invocation, alternate the builds under comparison, and
-- compare the diff child's own wall (`brokkr results UUID`):
--
--   brokkr bench --iterations 1 diff-text --new 320k
--   brokkr bench --iterations 1 diff-text --dataset germany --new 20260420 --verbose yes --common no
--
-- Read ordered_pool_result_send_wait_ns and diff_text_overflow_* on the
-- diff child to see whether units outran their channels.
--
-- Exit 0 (equal) and 1 (different) are both answers; anything else fails.
-- The output size and summary are recorded so arms can be checked for
-- parity.

local function resident(path)
    local r = binary_with({ program = "fincore" }, "--bytes", "--noheadings", "--output", "RES", path):ok()
    local res = tonumber(r.stdout:match("%d+"))
    if res == nil then fail("fincore gave no resident size for " .. path) end
    return res
end

function preflight(params)
    if params.old_variant.file == params.new_variant.file then
        fail("--old and --new name the same file")
    end
    if params.verbose ~= "yes" and params.verbose ~= "no" then
        fail("--verbose takes yes or no, not " .. params.verbose)
    end
    if params.common ~= "yes" and params.common ~= "no" then
        fail("--common takes yes or no, not " .. params.common)
    end
    for _, f in ipairs({ params.old_variant.file, params.new_variant.file }) do
        binary_with({ program = "dd" }, "if=" .. f, "of=/dev/null", "bs=4M", "status=none"):ok()
    end
end

function bench(params)
    local old = params.old_variant.file
    local new = params.new_variant.file
    for _, f in ipairs({ old, new }) do
        local res = resident(f.path)
        if res < filesize(f) then
            fail("only " .. res .. " bytes of " .. f.path .. " are cached in a warm run")
        end
    end
    local out = output("diff.txt")
    local flags = { output = out, overwrite = true }
    if params.verbose == "yes" then flags.verbose = true end
    if params.common == "no" then flags["suppress-common"] = true end
    local d = binary("diff", old, new, flags)
    record.diff_exit = d.exit
    if d.exit ~= 0 and d.exit ~= 1 then
        fail("diff exited " .. tostring(d.exit) .. ": " .. d.stderr)
    end
    record.output_bytes = filesize(out)
    record.summary = d.stderr
end
