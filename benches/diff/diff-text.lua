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
-- against 20260420 with -c -v (real verbose details). The diff call is
-- `warm` over both inputs (`brokkr man bench page-cache`): brokkr reads
-- both through after admitting the call and fails the iteration unless all
-- of both is resident before launch, so the diff times formatting and
-- scheduling, not the drive. The call is `measured`, so the result is the
-- diff child's wall, without that read. Take one iteration per invocation
-- and alternate the builds under comparison:
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

function preflight(params)
    for _, f in ipairs({ params.old_variant.file, params.new_variant.file }) do
        local why = page_cache(f, "warm")
        if why then fail(why) end
    end
end

-- --verbose and --common are checked by their choices in
-- brokkr.d/benches.toml.
function validate(params)
    if params.old_variant.file == params.new_variant.file then
        fail("--old and --new name the same file")
    end
end

function bench(params)
    local old = params.old_variant.file
    local new = params.new_variant.file
    local out = output("diff.txt")
    local flags = { output = out, overwrite = true }
    if params.verbose == "yes" then flags.verbose = true end
    if params.common == "no" then flags["suppress-common"] = true end
    local d = binary_with({ warm = { old, new }, measured = true }, "diff", old, new, flags)
    record.diff_exit = d.exit
    if d.exit ~= 0 and d.exit ~= 1 then
        fail("diff exited " .. tostring(d.exit) .. ": " .. d.stderr)
    end
    record.output_bytes = filesize(out)
    record.summary = d.stderr
end
