-- Multi-function test for tracemoon profiling
-- Run with: lua5.4 tests/lua/multi_func.lua

local function compute_heavy()
    local sum = 0
    for i = 1, 100000 do
        sum = sum + math.sin(i) * math.cos(i)
    end
    return sum
end

local function process_data()
    local result = 0
    for i = 1, 1000 do
        result = result + compute_heavy()
    end
    return result
end

local function main_loop()
    while true do
        process_data()
    end
end

print("Starting multi-function test...")
main_loop()
