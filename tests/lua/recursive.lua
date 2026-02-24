-- Recursive test script for tracemoon profiling
-- Run with: lua5.4 tests/lua/recursive.lua

function factorial(n)
    if n <= 1 then
        -- Spin when we hit the base case
        local sum = 0
        while true do
            sum = sum + 1
            if sum > 1000000000 then
                sum = 0
            end
        end
    end
    return n * factorial(n - 1)
end

print("Starting recursive test...")
factorial(20)
