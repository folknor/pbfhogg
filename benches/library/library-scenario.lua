-- library-scenario: time one pbfhogg-harness worker running one scenario.
-- The worker is launched in its bench mode, which lays the scenario's roles
-- out under the work directory itself and judges nothing; every bound PBF
-- variant is handed over and the scenario picks the ones it declares. The
-- measurement is the whole worker process; under --hotpath/--alloc the
-- profile is the whole worker too.
--
-- brokkr bench --iterations 1 library-scenario --scenario repeat-cat --dataset denmark

function bench(params, keys)
    local args = { "--bench-harness", params.scenario, "--work", scratch("work") }
    for i, key in ipairs(keys.inputs) do
        args[#args + 1] = "--dataset"
        args[#args + 1] = key
        args[#args + 1] = params.inputs[i].file
    end
    binary_with({ measured = true, hotpath = "worker" }, table.unpack(args)):ok()
end
