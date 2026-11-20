local skynet = require "skynet"
require "skynet.manager"
local name = ...

skynet.start(function()
    local sessions = {}
    for i = 1, 200 do
        sessions["uid:" .. i] = { payload = string.rep(name, 256) .. i }
    end
    local function flush()
        return sessions
    end
    package.loaded["tracemoon.fixture"] = { name = name, sessions = sessions, flush = flush }
    skynet.register(".trace_" .. name)
    collectgarbage("collect")
    collectgarbage("stop")
    skynet.error("TRACEMOON_READY:" .. name)
    -- Optional, test-only cooperative export for offline report previews on macOS.
    local export_dir = os.getenv("TRACEMOON_EXPORT_DIR")
    if export_dir then
        local exporter = assert(package.loadlib(assert(os.getenv("TRACEMOON_EXPORT_MODULE")),
            "luaopen_tracemoon_heap_export"))()
        exporter.dump(export_dir .. "/" .. name .. ".bin", skynet.self())
        skynet.error("TRACEMOON_EXPORTED:" .. name)
    end
    -- Let the start callback return so newservice can finish and bootstrap can
    -- launch the second VM. The work itself belongs to a separate coroutine.
    skynet.fork(function()
        while true do
            local result = 0
            for i = 1, 100000 do
                result = result + math.sin(i)
            end
            _G.tracemoon_result = result
            -- Deliberately stay CPU-bound: each of the two fixture VMs occupies a
            -- worker so successful sampling is not dependent on timer wakeup timing.
        end
    end)
end)
