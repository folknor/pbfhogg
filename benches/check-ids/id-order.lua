-- id-order: the typed `brokkr check-ids` workload, one `check --ids`
-- child per iteration. Streaming ordering checks are the default; --full
-- yes adds duplicate detection and its per-type ID sets. --direct-io yes
-- selects O_DIRECT reads. Dataset, snapshot and variant bind the input so
-- brokkr reserves, hashes and pairs the same file the typed runner uses.
-- No cache preparation is added: the typed command does none either.
--
-- Exit 0 (clean) and exit 1 (violations) both complete the iteration
-- (benches/lib/answer.lua); the answer is not judged. The child wall is
-- the matching measurement; the iteration wall also includes Lua
-- bookkeeping. The marked child supports bench --hotpath and --alloc.
-- Build features belong in the registry. Direct-I/O support is built for
-- both arms so only the read mode changes between them.
--
--   brokkr bench --iterations 1 id-order --dataset denmark --full yes
--   brokkr bench --dry-run id-order --full yes,no --direct-io yes,no

local answer = require("answer")

function bench(params)
    local r = binary_with({ hotpath = "check-ids" }, "check", "--ids", params.variant.file, {
        full = params.full == "yes",
        ["direct-io"] = params["direct-io"] == "yes",
    })
    answer.completed("check --ids", r)
    record.summary = r.stderr
end
