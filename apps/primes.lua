-- category: Examples
-- Primes: counts prime numbers forever (or up to arg[1]).
-- A long-running console program: try `primes &`, then `jobs` and `kill %1`,
-- or run it in the foreground and press Ctrl+C.

local limit = tonumber(arg[1]) or math.huge
local found, n = 0, 1
local next_report = sys.ticks() + 1000

while n < limit do
  n = n + 1
  local prime = true
  for d = 2, math.floor(math.sqrt(n)) do
    if n % d == 0 then prime = false break end
  end
  if prime then found = found + 1 end
  if sys.ticks() >= next_report then
    print(string.format("%d primes below %d", found, n))
    next_report = next_report + 1000
  end
end
print(string.format("%d primes below %d", found, limit))
