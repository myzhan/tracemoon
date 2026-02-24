-- Simple loop test script for tracemoon profiling
-- Run with: lua5.4 tests/lua/simple_loop.lua

function outer()
    inner()
end

function inner()
    local sum = 0
    while true do
        sum = sum + 1
        if sum > 1000000000 then
            sum = 0
        end
    end
end

print("Starting simple loop test...")
outer()
