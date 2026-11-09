-- index-integrity: the typed `brokkr check-index` workload, one
-- `check --index` child per iteration. This selector suppresses the default
-- IDs/refs checks, checking blob indexdata and tagdata against the payload.
-- Dataset, snapshot and variant bind the input for reservation, hashing and
-- pairing. No cache preparation: the typed runner does none either.
-- Child wall matches that workload; iteration wall includes Lua bookkeeping.
--
-- Exit 0 is clean, exit 1 violations, narrowed by expect.check-index when
-- declared. Before 854f9465, only 0 is trusted. Like ref-integrity, since()
-- uses ancestry rather than the typed runner's binary capability probe;
-- a backport of the exit split outside that ancestry can differ.
-- A marker stop records a partial run without claiming a full verdict.
-- The marked child supports --hotpath and --alloc, including marker stops.
-- io_uring and compression are absent because typed check-index refuses
-- them. Build features, commit selection and iteration counts belong to the
-- bench runner. Direct-I/O support is built for both read modes.
--
--   brokkr bench --dry-run index-integrity --direct-io yes,no
--   brokkr bench --alloc index-integrity --stop CHECK_INDEX

local function expected(params)
    local declared = params.variant.expect
    if declared then return declared["check-index"] end
end

local function stop_marker(value)
    local marker = value:gsub("^-", "")
    if marker:sub(-4) ~= "_END" then marker = marker .. "_END" end
    return marker
end

function validate(params)
    if params.stop then
        if not params.stop:match("^%-?[%w_]+$") or stop_marker(params.stop) == "_END" then
            fail("--stop must be a marker name: FOO_END, FOO or -FOO")
        end
    end
end

function preflight(params)
    if not params.stop and expected(params) == "violations" and not since("854f9465") then
        fail("declared violations cannot be trusted before the exit-code split (854f9465)")
    end
end

function bench(params)
    local options = { hotpath = "check-index" }
    if params.stop then options.stop_at = stop_marker(params.stop) end
    local r = binary_with(options, "check", "--index", params.variant.file, {
        ["direct-io"] = params["direct-io"] == "yes",
    })
    record.summary = r.stderr
    if params.verbose == "yes" then
        print(r.stdout)
        print(r.stderr)
    end
    if r.stopped_at then
        record.stopped_at = r.stopped_at
        return "stopped"
    end
    if r.exit ~= 0 and (r.exit ~= 1 or not since("854f9465")) then
        fail("check --index ended with exit " .. tostring(r.exit)
            .. ", signal " .. tostring(r.signal) .. ": " .. r.stderr)
    end
    local verdict = "clean"
    if r.exit == 1 then verdict = "violations" end
    local want = expected(params)
    if want and verdict ~= want then
        fail("check --index answered " .. verdict .. ", input expect.check-index declares " .. want)
    end
    record.check_exit = r.exit
    return verdict
end
