-- geocode-query: reverse geocoding query latency against an index this
-- bench builds. Each iteration builds the dataset's index with the built
-- revision's build-geocode-index (setup, outside the measurement), then runs
-- the hidden bench-geocode-query instrument once, the measured child: it
-- opens the index, draws a fixed pseudo-random point set uniform in the bbox
-- (a pure function of bbox and seed, the same on every revision), queries
-- every point once untimed to warm the page cache and tally the answers,
-- then times the given rounds over the same points.
--
-- The ranked wall is the query child's, so it includes process start, the
-- index open and the warm-up round; the per-query figure to compare is
-- record.child.us_per_query_best (fastest timed round's mean), with every
-- round's mean beside it. record.child.admin_digest hashes every query's
-- admin answers: two revisions whose digests differ answered differently.
-- A query error in the warm-up or any timed round fails the instrument, so
-- r:ok() fails the run: a revision whose queries fail never reads as faster.
--
-- The instrument must exist in the built revision: a --commit before it was
-- added fails with that reason. The bbox defaults to the dataset's bbox,
-- which is host config; pass --bbox to fix the points across hosts.
--
--   brokkr bench --dry-run geocode-query --dataset germany
--   brokkr bench --iterations 1 geocode-query --dataset germany --bbox 5.9,47.3,15.0,55.0

local bboxes = require("bbox")

-- --bbox, --queries, --rounds and --seed have typed domains in the
-- registry (bbox, positive u32, positive u32, u64), matching the
-- instrument's own argument types. What is left here is the effective
-- bbox's strict order, which the instrument would refuse only after the
-- index build.
function validate(params)
    local bbox = params.bbox or params.dataset.bbox
    if bbox == nil then
        fail("the dataset has no bbox in this host's config: pass --bbox")
    end
    bboxes.parse(bbox)
end

function bench(params)
    local bbox = params.bbox or params.dataset.bbox
    local index = output("geocode-index")
    binary("build-geocode-index", params.variant.file, {
        ["output-dir"] = index,
        ["tmp-dir"] = scratch("geocode"),
        force = true,
    }):ok()

    local r = binary_with({ measured = true, reads = { index } },
        "bench-geocode-query", index.path, {
            bbox = bbox,
            queries = params.queries,
            rounds = params.rounds,
            seed = params.seed,
        })
    if r.exit ~= 0 and string.find(r.stderr, "unrecognized subcommand", 1, true) then
        fail("this revision has no bench-geocode-query instrument")
    end
    r:ok()
    if params.verbose == "yes" then print(r.stderr) end

    -- This is the legacy instrument schema, not a full-answer digest.
    -- Keep parsing fail-closed until the instrument supplies a versioned
    -- full-answer result; never interpret its admin digest as that result.
    local fields = {
        elapsed_ms = true, open_ms = true, queries = true, rounds = true,
        us_per_query_best = true, us_per_query_rounds = true,
        admin_answered = true, admin_matches = true, addresses = true,
        streets = true, interpolations = true, admin_digest = true,
    }
    local child = { bbox = bbox, result_format = "legacy-admin-v1" }
    for line in string.gmatch(r.stderr, "[^\r\n]+") do
        local key, value = string.match(line, "^([%w_]+)=(.*)$")
        if key and fields[key] then
            if child[key] ~= nil then fail("duplicate geocode field: " .. key) end
            child[key] = value
        end
    end
    for key in pairs(fields) do
        if child[key] == nil then fail("missing geocode field: " .. key) end
    end
    for key in pairs(fields) do
        if key ~= "admin_digest" and key ~= "us_per_query_rounds" then
            local value = child[key]
            local pattern = key == "us_per_query_best" and "^%d+%.%d%d$" or "^%d+$"
            local n = tonumber(value)
            if not value:match(pattern) or not n or n == math.huge then
                fail("invalid geocode field: " .. key)
            end
            child[key] = n
        end
    end
    if #child.admin_digest ~= 16 or not child.admin_digest:match("^%x+$") then
        fail("invalid geocode admin_digest")
    end
    local count, best = 0, math.huge
    local values = child.us_per_query_rounds
    if values:sub(-1) == "," or values:sub(1, 1) == "," or values:find(",,", 1, true) then
        fail("invalid geocode round list")
    end
    for value in values:gmatch("[^,]+") do
        local n = tonumber(value)
        if not value:match("^%d+%.%d%d$") or not n or n == math.huge then
            fail("invalid geocode round latency")
        end
        count = count + 1
        best = math.min(best, n)
    end
    if child.queries ~= tonumber(params.queries) or child.rounds ~= tonumber(params.rounds)
        or count ~= child.rounds or best ~= child.us_per_query_best then
        fail("geocode workload or round summary mismatch")
    end
    for _, key in ipairs({ "admin_answered", "addresses", "streets", "interpolations" }) do
        if child[key] > child.queries then fail("geocode count exceeds queries: " .. key) end
    end
    if child.admin_matches < child.admin_answered then fail("geocode admin count mismatch") end
    record.child = child
end
