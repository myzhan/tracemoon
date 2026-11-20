local skynet = require "skynet"
require "skynet.manager"

skynet.start(function()
    local launcher = assert(skynet.launch("snlua", "launcher"))
    skynet.name(".launcher", launcher)
    skynet.newservice("tracemoon_worker", "alpha")
    skynet.newservice("tracemoon_worker", "beta")
    local core_trigger = os.getenv("TRACEMOON_CORE_TRIGGER")
    if core_trigger then
        assert(package.loadlib(core_trigger, "luaopen_tracemoon_core_trigger"))()
    end
    -- No sockets or external services: both named VMs remain alive for capture.
end)
