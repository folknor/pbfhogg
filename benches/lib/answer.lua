-- Completion of a child whose exit code carries an answer: check exits 0
-- for clean and 1 for violations, diff 0 for same and 1 for differs. Both
-- are completed workloads, so either completes the iteration; any other
-- ending fails it, as :ok() fails any non-zero exit of other commands.
-- Before 854f9465 exit 1 could also mean a failure, so there only 0
-- completes. since() uses commit ancestry, so a backport of the exit split
-- outside that ancestry can differ.
--
-- A bench records the measurement and nothing else: which answer the
-- child gave is not judged, returned or recorded here (brokkr stores every
-- child's ending with the run). Whether an answer is right is the
-- verifier's question, `brokkr run --with pbfhogg pbfhogg-verify --`.

local M = {}

function M.completed(command, r)
    if r.exit ~= 0 and (r.exit ~= 1 or not since("854f9465")) then
        fail(command .. " ended with exit " .. tostring(r.exit)
            .. ", signal " .. tostring(r.signal) .. ": " .. r.stderr)
    end
end

return M
