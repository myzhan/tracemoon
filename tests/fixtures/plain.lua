-- Stable business data for heap assertions, with a CPU workload that never exits early.
local sessions = {}
for i = 1, 200 do
    sessions["uid:" .. i] = { payload = string.rep("x", 1024) .. i }
end
local function flush()
    return sessions
end
package.loaded["tracemoon.fixture"] = { sessions = sessions, flush = flush }
collectgarbage("collect")
collectgarbage("stop")
io.stdout:write("TRACEMOON_READY:plain\n")
io.stdout:flush()
local core_trigger = os.getenv("TRACEMOON_CORE_TRIGGER")
if core_trigger then
    assert(package.loadlib(core_trigger, "luaopen_tracemoon_core_trigger"))()
end

local function compute()
    local result = 0
    for i = 1, 100000 do
        result = result + math.sin(i)
    end
    return result
end
while true do
    _G.tracemoon_result = compute()
end
