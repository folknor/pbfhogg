-- id-order: the typed `brokkr check-ids` workload, one `check --ids`
-- child per iteration. Streaming ordering checks are the default; --full
-- yes adds duplicate detection and its per-type ID sets. --direct-io yes
-- selects O_DIRECT reads. Dataset, snapshot and variant bind the input so
-- brokkr reserves, hashes and pairs the same file the typed runner uses.
-- No cache preparation is added: the typed command does none either.
--
-- Preserve the typed runner's answer policy: exit 0 means clean, exit 1
-- means violations, and an input's expect.check-ids narrows that to the
-- declared answer. Before 854f9465, exit 1 could also mean a failure, so
-- only exit 0 is trusted and a declared violations input is refused.
-- Lua uses since() for that boundary; the typed runner instead probes the
-- executable and caches its capability by digest. A backport outside that
-- ancestry can therefore differ. Probing inside bench() would add extra
-- pbfhogg children to every measured iteration, changing the workload.
-- The child wall is the matching measurement; the iteration wall also
-- includes Lua bookkeeping. The marked child supports bench --hotpath.
-- Script benches have no typed --alloc mode or arbitrary --features
-- override; build features belong in the registry. Direct-I/O support is
-- built for both arms so only the read mode changes between them.
--
--   brokkr bench --iterations 1 id-order --dataset denmark --full yes
--   brokkr bench --dry-run id-order --full yes,no --direct-io yes,no

local function expected(params)
    local declared = params.variant.expect
    if declared then return declared["check-ids"] end
end

function preflight(params)
    if expected(params) == "violations" and not since("854f9465") then
        fail("declared violations cannot be trusted before the exit-code split (854f9465)")
    end
end

function bench(params)
    local split = since("854f9465")
    local r = binary_with({ hotpath = "check-ids" }, "check", "--ids", params.variant.file, {
        full = params.full == "yes",
        ["direct-io"] = params["direct-io"] == "yes",
    })
    if r.exit ~= 0 and (r.exit ~= 1 or not split) then
        fail("check --ids ended with exit " .. tostring(r.exit)
            .. ", signal " .. tostring(r.signal) .. ": " .. r.stderr)
    end
    local verdict = "clean"
    if r.exit == 1 then verdict = "violations" end
    local want = expected(params)
    if want and verdict ~= want then
        fail("check --ids answered " .. verdict .. ", input expect.check-ids declares " .. want)
    end
    record.check_exit = r.exit
    record.summary = r.stderr
    return verdict
end
