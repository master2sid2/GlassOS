# net — network

## net.fetch(url [, fn])

An HTTP GET request. Redirects are followed automatically. HTTPS is not supported yet: such an address returns an error.

**In a windowed program** always pass a function: the window must not wait for the network. The answer comes to it, and the window is redrawn.

```lua
net.fetch("http://example.com/", function(body, status)
  if body then
    state.text = "HTTP " .. status .. ", " .. #body .. " bytes"
  else
    state.text = "Error: " .. status
  end
end)
```

On success `fn` gets the body and the status code. On failure — `nil` and an error message instead of the code. A 404 is not an error: check `status`.

Without a function, a window gets a Lua error.

**In a console program run from the terminal** the answer can be awaited right in the code:

```lua
local body, status = net.fetch("http://example.com/")
print(body and #body or status)
```

The whole answer is kept in memory, its size is limited to 16 MiB: a larger answer returns the error `response is too large`.

## net.hostname() → name, workgroup

The name of this computer on the network and its workgroup. They are set in Settings → Network, the Computer tab.

```lua
local name, group = net.hostname()
print(name .. " in " .. group)
```

The list of other computers is the `/network` folder, see [fs](04-fs.md).
