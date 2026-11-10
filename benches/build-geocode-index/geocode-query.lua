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
--
-- The instrument must exist in the built revision: a --commit before it was
-- added fails with that reason. The bbox defaults to the dataset's bbox,
-- which is host config; pass --bbox to fix the points across hosts.
--
--   brokkr bench --dry-run geocode-query --dataset germany
--   brokkr bench --iterations 1 geocode-query --dataset germany --bbox 5.9,47.3,15.0,55.0

local function bbox_ok(s)
    local parts = {}
    for v in string.gmatch(s, "[^,]+") do parts[#parts + 1] = tonumber(v) end
    if #parts ~= 4 then return false end
    for i = 1, 4 do if parts[i] == nil then return false end end
    return parts[1] < parts[3] and parts[2] < parts[4]
end

local function positive_int(s)
    return s ~= nil and string.match(s, "^[1-9]%d*$") ~= nil
end

function validate(params)
    if params.bbox and not bbox_ok(params.bbox) then
        fail("--bbox must be min_lon,min_lat,max_lon,max_lat with min below max")
    end
    if params.bbox == nil and params.dataset.bbox == nil then
        fail("the dataset has no bbox in this host's config: pass --bbox")
    end
    if not positive_int(params.queries) then fail("--queries must be a positive integer") end
    if not positive_int(params.rounds) then fail("--rounds must be a positive integer") end
    if not string.match(params.seed, "^%d+$") then fail("--seed must be an unsigned integer") end
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

    local child = { bbox = bbox }
    for key, value in string.gmatch(r.stderr, "([%w_]+)=([^\n]+)") do
        child[key] = tonumber(value) or value
    end
    -- Comma lists and hex digests stay text; the digest must never become a
    -- number, which would lose bits.
    child.admin_digest = string.match(r.stderr, "admin_digest=(%x+)")
    child.us_per_query_rounds = string.match(r.stderr, "us_per_query_rounds=([^\n]+)")
    if child.us_per_query_best == nil then fail("bench-geocode-query reported no us_per_query_best") end
    record.child = child
end
