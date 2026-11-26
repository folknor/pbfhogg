-- geocode-query: reverse geocoding query latency against an index this
-- bench builds. Builds the pbfhogg-bench package's instruments, not the
-- pbfhogg CLI. Each iteration builds the dataset's index with the
-- instruments' geocode-build (setup, outside the measurement: the library
-- builder with its defaults, an unindexed input accepted), then runs
-- geocode-query once, the measured child: it opens the index, draws a fixed
-- pseudo-random point set uniform in the bbox (a pure function of bbox and
-- seed, the same on every revision), queries every point once untimed to
-- warm the page cache, then times the given rounds
-- over the same points. Its warm-up and timed rounds are the marker pairs
-- GEOCODE_QUERY_WARMUP and GEOCODE_QUERY_ROUNDS.
--
-- The ranked wall is the query child's, so it includes process start, the
-- index open and the warm-up round; the per-query figure to compare is
-- record.child.us_per_query_best (fastest timed round's mean), with every
-- round's mean beside it. The answers are not judged or recorded; each
-- whole answer goes to an opaque consumer so none of it is optimized away.
-- What this measures is serial library query latency over uniform points
-- whose pages the warm-up already touched: not cold-page latency, not an
-- address-dense load, and not CLI throughput, which are other workloads.
-- A query error in the warm-up or any timed round fails the instrument, so
-- r:ok() fails the run: a revision whose queries fail never reads as faster.
-- Under --hotpath or --alloc the query child is profiled; Reader::query
-- and its level, admin and interpolation searches carry the scopes, and
-- the profile covers the warm-up as well as the timed rounds.
--
-- A --commit before the pbfhogg-bench package existed fails at the build.
-- The bbox defaults to the dataset's bbox, which is host config; pass
-- --bbox to fix the points across hosts.
--
--   brokkr bench --dry-run geocode-query --dataset germany
--   brokkr bench --iterations 1 geocode-query --dataset germany --bbox 5.9,47.3,15.0,55.0

local geocode = require("geocode")

-- --bbox, --queries, --rounds and --seed have typed domains in the
-- registry (bbox, positive u32, positive u32, u64), matching the
-- instrument's own argument types. What is left here is the effective
-- bbox's strict order, which the instrument would refuse only after the
-- index build.
function validate(params)
    geocode.bbox(params)
end

function bench(params)
    local bbox = params.bbox or params.dataset.bbox
    -- Rebuild per iteration: brokkr#147 declined a persistent producer cache.
    -- Iteration outputs are reclaimed; local reuse would bypass ownership and
    -- invalidation by input digest, producer revision and build options.
    local index = output("geocode-index")
    binary("geocode-build", params.variant.file, {
        ["output-dir"] = index,
        ["tmp-dir"] = scratch("geocode"),
    }):ok()

    local r = binary_with({ measured = true, reads = { index }, hotpath = "query" },
        "geocode-query", index.path, {
            bbox = bbox,
            queries = params.queries,
            rounds = params.rounds,
            seed = params.seed,
        }):ok()
    if params.verbose == "yes" then print(r.stderr) end

    -- The instrument's measurement fields, each exactly once. The workload
    -- echo and round summary must describe the requested work, so a
    -- measurement of some other workload is never recorded as this one.
    local fields = {
        elapsed_ms = true, open_ms = true, queries = true, rounds = true,
        us_per_query_best = true, us_per_query_rounds = true,
    }
    local child = { bbox = bbox }
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
        if key ~= "us_per_query_rounds" then
            local value = child[key]
            local pattern = key == "us_per_query_best" and "^%d+%.%d%d$" or "^%d+$"
            local n = tonumber(value)
            if not value:match(pattern) or not n or n == math.huge then
                fail("invalid geocode field: " .. key)
            end
            child[key] = n
        end
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
    record.child = child
end
