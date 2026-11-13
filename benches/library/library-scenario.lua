-- library-scenario: measure the harness worker directly, never a verifier.
-- Discovery and request/seed preparation are outside the measured child.
-- Dataset roles use their declared variants in the bound snapshot, including
-- mixed raw/indexed scenarios. --variant never replaces those declarations.
-- Generated files start absent; the worker creates them. Output seeds use the
-- protocol recipe exactly. All files are iteration-owned and discarded.
-- Only hotpath-declared scenarios mark a capture. Under --hotpath/--alloc an
-- ineligible scenario fails after its first worker run (brokkr#176); timing
-- and profiling always launch the same workload. Deadlines are brokkr-owned.
-- Verification is exclusively `brokkr run ... pbfhogg-library -- verify ...`.
--
-- brokkr bench --iterations 1 library-scenario --scenario repeat-cat --dataset denmark --variant indexed

function bench(params, keys)
    local discovery = binary_with({ deadline = 60 }, "--describe-harness"):ok()
    if discovery.stdout_truncated then fail("harness describe exceeds stdout preview") end
    local describe = json.decode(discovery.stdout)
    if describe.protocol ~= 1 then fail("unsupported harness describe protocol") end
    local scenario = nil
    for _, candidate in ipairs(describe.scenarios) do
        if candidate.id == params.scenario then
            if scenario then fail("duplicate scenario " .. params.scenario) end
            scenario = candidate
        end
    end
    if not scenario then fail("unknown scenario " .. params.scenario) end
    if type(scenario.deadline_s) ~= "number" or scenario.deadline_s <= 0 then
        fail("scenario needs a positive deadline_s")
    end
    local variants = {}
    for i, key in ipairs(keys.inputs) do variants[key] = params.inputs[i] end
    local bindings, reads, writes = {}, {}, {}
    for _, role in ipairs(scenario.roles) do
        if bindings[role.id] then fail("duplicate role " .. role.id) end
        local handle = nil
        if role.source == "dataset" and role.kind == "pbf" then
            local entry = variants[role.variant]
            if not entry or not entry.file then fail("missing declared PBF variant " .. tostring(role.variant)) end
            handle = entry.file
            reads[#reads + 1] = handle
        elseif role.source == "output" or role.source == "generated" then
            if role.kind == "pbf" or role.kind == "file" then
                handle = output("role-" .. role.id)
            elseif role.kind == "directory" then
                if role.seeded == true then fail("seeded directory roles are not supported") end
                handle = scratch("role-" .. role.id)
            else
                fail("unsupported role kind " .. tostring(role.kind))
            end
            if role.seeded == true then
                if role.source ~= "output" then fail("only output roles can be seeded") end
                write(handle, "brokkr-library-seed-v1 " .. role.id .. "\n")
            end
            writes[#writes + 1] = handle
        else
            fail("unsupported role source/kind " .. tostring(role.source) .. "/" .. tostring(role.kind))
        end
        bindings[role.id] = handle.path
    end
    local request = output("request.json")
    write(request, json.encode({ protocol = 1, scenario = scenario.id, iteration = 0, bindings = bindings }))
    reads[#reads + 1] = request
    local private = scratch("private")
    local artefacts = scratch("artefacts")
    writes[#writes + 1] = private
    writes[#writes + 1] = artefacts
    binary_with({ measured = true, deadline = scenario.deadline_s,
        hotpath = scenario.hotpath == true and "worker" or nil,
        reads = reads, writes = writes },
        "--test-harness", request, "--private-dir", private, "--artefact-dir", artefacts):ok()
end
