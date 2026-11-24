-- default-integrity: a bare `check`, the ID and reference checks the
-- command runs with no selector, one child per iteration. Optional
-- relation-member checks and duplicate detection, as the two checks take
-- them. Bind dataset, snapshot and variant for input reservation, hashing
-- and pairing. No cache preparation.
--
-- Exit 0 (clean) and exit 1 (violations) both complete the iteration
-- (benches/lib/answer.lua); the answer is not judged.
-- The marked child supports --hotpath and --alloc.
--
--   brokkr bench --dry-run default-integrity --check-relations yes,no
--   brokkr bench --iterations 1 default-integrity --dataset planet

local answer = require("answer")

function bench(params)
    local r = binary_with({ hotpath = "check" }, "check", params.variant.file, {
        ["check-relations"] = params["check-relations"] == "yes",
        full = params.full == "yes",
    })
    answer.completed("check", r)
    record.summary = r.stderr
end
