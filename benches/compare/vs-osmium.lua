-- vs-osmium: one row of the guide's "CLI command benchmarks" table
-- (docs/guide/performance.md), timed for pbfhogg and for osmium on the same
-- input and the same requested result. Registered in brokkr.d/benches.toml.
-- Each row's two command lines are written side by side below, so a reader
-- can check they ask for the same thing. `--tool` takes pbfhogg, osmium or
-- both. Every iteration starts with its inputs out of the page cache (see
-- inputs below), so the iterations of one invocation are alike. The published figures come from alternating
-- single-iteration runs, the starting tool reversed from pair to pair
-- (scripts/vs-osmium.sh), medians compared:
--
--   brokkr bench --iterations 1 vs-osmium --row sort --tool pbfhogg
--   brokkr bench --iterations 1 vs-osmium --row sort --tool osmium
--   brokkr bench --iterations 1 vs-osmium --row apply-changes --osc 4705 --tool osmium
--
-- Both tools read the same file (the indexed variant by default; osmium
-- ignores the indexdata, and making that file is not timed). Explicit
-- compression on both sides is zlib level 6 where encoding is needed. Where the defaults differ in
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
-- Timing runs certify execution only. --validate-only yes separately runs
-- both PBF-producing arms, decodes both with osmium to OPL (including way
-- locations), and compares the complete spools with cmp. This checks ordered
-- entities, metadata, tags, references and locations, but excludes headers
-- and encoding. Tag order is significant under this deliberately strict policy.
-- Text-report rows have different report formats and refuse validation.
-- The CLI exposes no affirmative complete-ways or keep-member-nodes switch:
-- their defaults are part of the compared behavior, checked by validation.
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
local function rows(params, keys, output_name)
    local pbf = params.variant.file
    local out = function() return output(output_name or "out.osm.pbf") end
    local pbf_flags = function(flags)
        flags = flags or {}
        flags.compression = "zlib:6"
        flags.output = out()
        return flags
    end
    local osm = function(flags)
        flags.fsync = true
        flags["output-format"] = "pbf,pbf_compression=zlib,pbf_compression_level=6"
        flags.output = out()
        return flags
    end
    return {
        ["inspect"] = {
            pbfhogg = function() return { "inspect", pbf } end,
        },
        ["sort"] = {
            pbfhogg = function() return { "sort", pbf, pbf_flags() } end,
            osmium = function() return { "sort", pbf, osm({}) } end,
        },
        ["extract"] = {
            pbfhogg = function() return { "extract", pbf, "-b=" .. params.dataset.bbox, pbf_flags() } end,
            osmium = function() return { "extract", pbf, "--bbox=" .. params.dataset.bbox, osm({ strategy = "complete_ways" }) } end,
        },
        ["apply-changes"] = {
            pbfhogg = function() return { "apply-changes", pbf, params.osc.file, pbf_flags() } end,
            osmium = function() return { "apply-changes", pbf, params.osc.file, osm({}) } end,
        },
        ["tags-filter-highway"] = {
            pbfhogg = function() return { "tags-filter", pbf, "w/highway=primary", pbf_flags({ R = true }) } end,
            osmium = function() return { "tags-filter", pbf, "w/highway=primary", osm({ R = true }) } end,
        },
        ["tags-filter-restaurant"] = {
            pbfhogg = function() return { "tags-filter", pbf, "amenity=restaurant", pbf_flags({ R = true }) } end,
            osmium = function() return { "tags-filter", pbf, "amenity=restaurant", osm({ R = true }) } end,
        },
        ["cat-way"] = {
            pbfhogg = function() return { "cat", pbf, pbf_flags({ type = "way" }) } end,
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
            pbfhogg = function() return getid_args(keys.dataset, pbf, pbf_flags()) end,
            osmium = function() return getid_args(keys.dataset, pbf, osm({})) end,
        },
        ["altw-sparse"] = {
            pbfhogg = function() return { "add-locations-to-ways", pbf, pbf_flags({ ["index-type"] = "sparse" }) } end,
            osmium = function() return { "add-locations-to-ways", pbf, osm({ ["keep-member-nodes"] = true }) } end,
        },
        ["altw-external"] = {
            pbfhogg = function() return { "add-locations-to-ways", pbf, pbf_flags({ ["index-type"] = "external" }) } end,
            osmium = function() return { "add-locations-to-ways", pbf, osm({ ["keep-member-nodes"] = true }) } end,
        },
    }
end

-- Both arms start cold: libosmium drops its input from the page cache as
-- it reads (posix_fadvise DONTNEED, unless OSMIUM_CLEAN_PAGE_CACHE_AFTER_READ
-- is no), so a run after an osmium run starts cold and one after a pbfhogg
-- run starts warm. So the tool's call is `cold` over every input of the arm
-- (`brokkr man bench page-cache`): brokkr advises each out after admitting
-- the call and fails the iteration unless none of any is resident before
-- launch. The call is `measured`, so the result is the tool child's wall,
-- without that preparation.
local function inputs(params)
    if params.osc then return { params.variant.file, params.osc.file } end
    return { params.variant.file }
end

function preflight(params)
    if params["validate-only"] == "yes" then return end
    for _, f in ipairs(inputs(params)) do
        local why = page_cache(f, "cold")
        if why then fail(why) end
    end
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
    if params["validate-only"] == "yes" and
        (params.row == "inspect" or params.row == "inspect-tags" or params.row == "inspect-tags-way") then
        fail("output validation requires a PBF-producing row")
    end
    if params.tool == "osmium" and not row.osmium then
        fail("the " .. params.row .. " row has no osmium counterpart")
    end
end

local function validate_outputs(params, keys)
    local left_args = rows(params, keys, "pbfhogg.osm.pbf")[params.row].pbfhogg()
    local right_args = rows(params, keys, "osmium.osm.pbf")[params.row].osmium()
    binary(table.unpack(left_args)):ok()
    binary_with({ program = "osmium" }, table.unpack(right_args)):ok()
    local function decode(args)
        local pbf = args[#args].output
        return binary_with({ program = "osmium", reads = { pbf } },
            "cat", pbf, { ["output-format"] = "opl,locations_on_ways=true" }):ok().stdout_file
    end
    local left, right = decode(left_args), decode(right_args)
    binary_with({ program = "cmp", reads = { left, right } }, left, right):ok()
    record.validation = "osmium-opl-ordered-v1"
end

function bench(params, keys)
    if params["validate-only"] == "yes" then
        validate_outputs(params, keys)
        return
    end
    local row = rows(params, keys)[params.row]
    local options = { cold = inputs(params), measured = true }
    if params.tool == "pbfhogg" then
        binary_with(options, table.unpack(row.pbfhogg())):ok()
    else
        options.program = "osmium"
        binary_with(options, table.unpack(row.osmium())):ok()
    end
end
