-- category: System
-- Console utility: prints system information. Run it from the terminal: sysinfo
local t = sys.time()
local up = sys.ticks() // 1000
local used, free = sys.mem()

print(string.format("GlassOS %s, %s", sys.version(), _VERSION))
print(string.format("Time:    %02d:%02d:%02d  %02d.%02d.%d", t.hour, t.min, t.sec, t.day, t.month, t.year))
print(string.format("Uptime:  %dm %ds", up // 60, up % 60))
print(string.format("Memory:  %s used, %s free of %s", sys.size(used * 1024), sys.size(free * 1024), sys.size((used + free) * 1024)))
print("Disks:")
for _, v in ipairs(fs.volumes()) do
  if not v.removable and not v.network then
    print(string.format("  %-16s %-6s %s used of %s, %s free", v.name, v.kind, sys.size(v.used), sys.size(v.total), sys.size(v.total - v.used)))
  end
end
print("Programs in /apps:")
for _, e in ipairs(fs.list("/apps")) do
  print(string.format("  %-14s %8s", e.name, sys.size(e.size)))
end
print(string.format("Math:    pi = %.6f, sqrt(2) = %.4f, 2^20 = %d", math.pi, math.sqrt(2), 2 ^ 20))
