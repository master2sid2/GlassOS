-- category: Examples
-- The smallest program: prints a greeting and shows a notification. Try: hello Alice
local name = arg[1] or "world"
print("Hello, " .. name .. "!")
sys.notify("Hello, " .. name .. "!", "hello.lua")
