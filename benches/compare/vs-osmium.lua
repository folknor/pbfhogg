-- vs-osmium: one row of the guide's "CLI command benchmarks" table
-- (docs/guide/performance.md), timed for pbfhogg and for osmium on the same
-- input and the same requested result. Registered in brokkr.d/benches.toml.
-- Each row's two command lines are written side by side below, so a reader
-- can check they ask for the same thing. `--tool` takes pbfhogg, osmium or
-- both. Every iteration starts with its inputs out of the page cache (see
-- evict below), so the iterations of one invocation are alike. The published figures come from alternating
-- single-iteration runs, the starting tool reversed from pair to pair
-- (scripts/vs-osmium.sh), medians compared:
--
--   brokkr bench --iterations 1 vs-osmium --row sort --tool pbfhogg
--   brokkr bench --iterations 1 vs-osmium --row sort --tool osmium
--   brokkr bench --iterations 1 vs-osmium --row apply-changes --osc 4705 --tool osmium
--
-- Both tools read the same file (the indexed variant by default; osmium
-- ignores the indexdata, and making that file is not timed). Default
-- compression on both sides is zlib level 6. Where the defaults differ in
-- what is produced or promised, osmium gets the flag that matches pbfhogg:
-- --fsync on every row that writes a PBF, since pbfhogg syncs its output
-- (and, beyond what osmium does, the output's directory) before it exits;
-- --keep-member-nodes on add-locations-to-ways, since pbfhogg always keeps
-- relation-member nodes. The inspect tags rows are bare on both sides and
-- print their full report to stdout, which brokkr spools. getid uses the
-- dataset's getid_ids, else the Denmark IDs the typed getid bench uses;
-- osmium exits 1 on an ID it cannot find, which fails the run, so an osmium
-- run that passes proves every ID exists. The inspect row has no osmium
-- counterpart: bare inspect answers from indexdata, and `osmium fileinfo -e`
-- computes a census (timestamps, bbox, metadata coverage) pbfhogg does not.
-- extract uses the dataset bbox and the complete-ways strategy on both
-- sides (pbfhogg's default, osmium's explicit complete_ways).

local DENMARK_IDS = { "n115722", "n115723", "n115724", "w2080", "w2081", "w2082", "r174", "r213", "r339" }

local function getid_args(dataset, pbf, flags)
    local ds = config().datasets[dataset]
    local ids = DENMARK_IDS
    if ds and ds.getid_ids then ids = ds.getid_ids end
    local args = { "getid", pbf }
    for _, id in ipairs(ids) do args[#args + 1] = id end
    args[#args + 1] = flags
    return args
end

-- Each row: the pbfhogg argv and the osmium argv. A row whose osmium side
-- is nil has no osmium counterpart.
local function rows(params, keys)
    local pbf = params.variant.file
    local out = function() return output("out.osm.pbf") end
    local osm = function(flags) flags.fsync = true; flags.output = out(); return flags end
    return {
        ["inspect"] = {
            pbfhogg = function() return { "inspect", pbf } end,
        },
        ["sort"] = {
            pbfhogg = function() return { "sort", pbf, { output = out() } } end,
            osmium = function() return { "sort", pbf, osm({}) } end,
        },
        ["extract"] = {
            pbfhogg = function() return { "extract", pbf, "-b=" .. params.dataset.bbox, { output = out() } } end,
            osmium = function() return { "extract", pbf, "--bbox=" .. params.dataset.bbox, osm({ strategy = "complete_ways" }) } end,
        },
        ["apply-changes"] = {
            pbfhogg = function() return { "apply-changes", pbf, params.osc.file, { output = out() } } end,
            osmium = function() return { "apply-changes", pbf, params.osc.file, osm({}) } end,
        },
        ["tags-filter-highway"] = {
            pbfhogg = function() return { "tags-filter", pbf, "w/highway=primary", { R = true, output = out() } } end,
            osmium = function() return { "tags-filter", pbf, "w/highway=primary", osm({ R = true }) } end,
        },
        ["tags-filter-restaurant"] = {
            pbfhogg = function() return { "tags-filter", pbf, "amenity=restaurant", { R = true, output = out() } } end,
            osmium = function() return { "tags-filter", pbf, "amenity=restaurant", osm({ R = true }) } end,
        },
        ["cat-way"] = {
            pbfhogg = function() return { "cat", pbf, { type = "way", output = out() } } end,
            osmium = function() return { "cat", pbf, osm({ t = "way" }) } end,
        },
        ["inspect-tags"] = {
            pbfhogg = function() return { "inspect", "tags", pbf } end,
            osmium = function() return { "tags-count", pbf } end,
        },
        ["inspect-tags-way"] = {
            pbfhogg = function() return { "inspect", "tags", pbf, { type = "way" } } end,
            osmium = function() return { "tags-count", pbf, { t = "way" } } end,
        },
        ["getid"] = {
            pbfhogg = function() return getid_args(keys.dataset, pbf, { output = out() }) end,
            osmium = function() return getid_args(keys.dataset, pbf, osm({})) end,
        },
        ["altw-sparse"] = {
            pbfhogg = function() return { "add-locations-to-ways", pbf, { ["index-type"] = "sparse", output = out() } } end,
            osmium = function() return { "add-locations-to-ways", pbf, osm({ ["keep-member-nodes"] = true }) } end,
        },
        ["altw-external"] = {
            pbfhogg = function() return { "add-locations-to-ways", pbf, { ["index-type"] = "external", output = out() } } end,
            osmium = function() return { "add-locations-to-ways", pbf, osm({ ["keep-member-nodes"] = true }) } end,
        },
    }
end

-- Both arms start cold: libosmium drops its input from the page cache as
-- it reads (posix_fadvise DONTNEED, unless OSMIUM_CLEAN_PAGE_CACHE_AFTER_READ
-- is no), so a run after an osmium run starts cold and one after a pbfhogg
-- run starts warm. The timed block drops every input of the arm the same
-- way just before the measured child (GNU dd's nocache with count=0 advises
-- the whole file away) and fails unless fincore then finds none of it
-- resident; the iteration wall includes that.
local function evict(path)
    binary_with({ program = "dd" }, "if=" .. path, "iflag=nocache", "count=0", "status=none"):ok()
    local r = binary_with({ program = "fincore" }, "--bytes", "--noheadings", "--output", "RES", path):ok()
    local res = tonumber(r.stdout:match("%d+"))
    if res == nil or res > 0 then fail("after eviction " .. tostring(res) .. " bytes of " .. path .. " are still cached") end
end

-- --row against the one row table bench() runs from, and the rules between
-- params; --tool is checked by its choices in brokkr.d/benches.toml. rows()
-- builds closures only, so nothing here launches or makes a handle.
function validate(params, keys)
    local row = rows(params, keys)[params.row]
    if not row then fail("unknown --row " .. params.row) end
    if params.row == "extract" and type(params.dataset.bbox) ~= "string" then
        fail("the extract row needs a configured dataset bbox")
    end
    if params.row == "apply-changes" and not params.osc then fail("the apply-changes row needs --osc") end
    if params.row ~= "apply-changes" and params.osc then fail("--osc is for the apply-changes row only") end
    if params.tool == "osmium" and not row.osmium then
        fail("the " .. params.row .. " row has no osmium counterpart")
    end
end

function bench(params, keys)
    local row = rows(params, keys)[params.row]
    evict(params.variant.file.path)
    if params.osc then evict(params.osc.file.path) end
    if params.tool == "pbfhogg" then
        binary(table.unpack(row.pbfhogg())):ok()
    else
        binary_with({ program = "osmium" }, table.unpack(row.osmium())):ok()
    end
end
