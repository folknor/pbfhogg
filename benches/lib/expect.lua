-- The declared-answer policy shared by the check integrity benches. The
-- calling script picks the expectation key from its own mode, because one
-- check can need different keys: brokkr reads check-refs for a plain ref
-- check and check-refs-relations under --check-relations, never one for the
-- other, since clean way refs say nothing about relation members. Every
-- function here takes that key, and every message names it, so the key a
-- run was judged by is the key its failure reports.
--
-- Exit 0 is clean and exit 1 violations. Before 854f9465 exit 1 could also
-- mean a failure, so only 0 is trusted and a declared violations input is
-- refused. since() uses commit ancestry, so a backport of the exit split
-- outside that ancestry can differ. An absent key accepts either answer
-- unless --require-expectation yes.
-- Configuration-wide orphan detection belongs to brokkr, which owns the
-- dataset schema and digest registration, rather than this per-cell lookup.

local M = {}

-- Apply one lookup policy to integrity and diff declarations alike. Record
-- selection separately from verdicts so undeclared cells remain identifiable.
function M.selected(params, key, want)
    record.expectation_key = key
    record.expectation_applied = want ~= nil
    if want ~= nil then record.expectation_answer = want end
    if want == nil and params["require-expectation"] == "yes" then
        fail("missing expect." .. key .. " required by --require-expectation yes")
    end
    return want
end

-- The answer the input declares under key, or nil.
function M.declared(params, key)
    local declared = params.variant.expect
    return M.selected(params, key, declared and declared[key])
end

-- Preflight refusal of a declared violations answer that the build
-- cannot be trusted to give.
function M.refuse_untrusted(params, key)
    if M.declared(params, key) == "violations" and not since("854f9465") then
        fail("expect." .. key .. " declares violations, which cannot be trusted"
            .. " before the exit-code split (854f9465)")
    end
end

-- The verdict of a finished check child r, failing on an untrusted exit or
-- on an answer the input's declaration under key contradicts.
function M.verdict(params, key, command, r)
    if r.exit ~= 0 and (r.exit ~= 1 or not since("854f9465")) then
        fail(command .. " ended with exit " .. tostring(r.exit)
            .. ", signal " .. tostring(r.signal) .. ": " .. r.stderr)
    end
    local verdict = "clean"
    if r.exit == 1 then verdict = "violations" end
    local want = M.declared(params, key)
    if want and verdict ~= want then
        fail(command .. " answered " .. verdict .. ", input expect." .. key
            .. " declares " .. want)
    end
    return verdict
end

return M
