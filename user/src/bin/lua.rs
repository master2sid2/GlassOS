//! /bin/lua — the GlassOS program interpreter. Each Lua program lives in
//! its own ring 3 process: if it loops or crashes, only it suffers.
//! Files, the network and windows go through the kernel (see src/lua/wire.rs). The kernel draws widgets into
//! the window's back buffer; the compositor shows the last finished frame and does not wait for the program.

#![no_std]
#![no_main]

extern crate alloc;

#[path = "../ffi.rs"]
#[allow(dead_code)]
mod ffi;
#[path = "../../../src/lua/wire.rs"]
mod wire;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::ffi::{c_char, c_int};
use ffi::*;
use wire::{OPS, SHORT_REPLY, Value, decode, encode};

glass::entry!(main);

/// The program memory limit.
const MEMORY_LIMIT: usize = 1 << 40;
/// The index of a C function's first upvalue (LUA_REGISTRYINDEX - 1).
const UPVALUE: c_int = -1_001_001;
const LUA_TNIL: c_int = 0;
const LUA_TBOOLEAN: c_int = 1;
const LUA_TNUMBER: c_int = 3;
const LUA_TSTRING: c_int = 4;

/// Lua code that runs before the program.
const PRELUDE: &str = r#"
function fs.list(path)
  local names, dirs, sizes = fs._list(path)
  if not names then return nil, dirs end
  local out = {}
  for i = 1, #names do
    out[i] = { name = names[i], dir = dirs[i] == "1", size = tonumber(sizes[i]) }
  end
  return out
end
function audio.status(id)
  local state, pos, len = audio._status(id)
  if not state then return nil, pos end
  return { state = state, pos = pos, len = len }
end
function audio.info(id)
  local format, title, artist, rate, channels, album = audio._info(id)
  if not format then return nil, title end
  return { format = format, title = title, artist = artist, rate = rate, channels = channels, album = album }
end
function audio.spectrum(id, bands)
  local s = audio._spectrum(id, bands or 16)
  if not s then return nil end
  return { s:byte(1, -1) }
end
function audio.probe(path)
  local len, title, artist, album = audio._probe(path)
  if not len then return nil, title end
  return { len = len, title = title, artist = artist, album = album }
end
function fs.volumes()
  local names, paths, kinds, used, total, memory, removable, network = fs._volumes()
  local out = {}
  for i = 1, #names do
    out[i] = {
      name = names[i], path = paths[i], kind = kinds[i],
      used = tonumber(used[i]), total = tonumber(total[i]),
      memory = memory[i] == "1", removable = removable[i] == "1", network = network[i] == "1",
    }
  end
  return out
end
function sys.size(bytes)
  if bytes < 1024 then return string.format("%d B", bytes) end
  local units, unit = { "KB", "MB", "GB", "TB" }, 1
  local value = bytes / 1024
  while value >= 1024 and unit < #units do value, unit = value / 1024, unit + 1 end
  return string.format(value >= 9.95 and "%.0f %s" or "%.1f %s", value, units[unit])
end
function sys.time()
  local hour, min, sec, day, month, year, wday, utc, offset, summer, zone = sys._time()
  return { hour = hour, min = min, sec = sec, day = day, month = month, year = year,
           wday = wday, utc = utc, offset = offset, summer = summer, zone = zone }
end
-- Modules: require("name") looks for name.lua next to the program and in package.path;
-- dots in the name are folders. A module runs once, its result is remembered.
package = { loaded = {}, path = "/apps/lib/?.lua;" .. select(2, sys.user()) .. "/lib/?.lua" }
function require(name)
  local known = package.loaded[name]
  if known ~= nil then return known end
  local file, tried = name:gsub("%.", "/"), {}
  local own = arg and arg[0] and arg[0]:match("^(.*)/[^/]*$")
  local path = (own and own .. "/?.lua;" or "") .. package.path
  for pattern in path:gmatch("[^;]+") do
    local candidate = pattern:gsub("%?", file)
    if fs.exists(candidate) and not fs.isdir(candidate) then
      local chunk, err = load(assert(fs.read(candidate)), "@" .. candidate)
      if not chunk then error(err, 2) end
      local result = chunk(name, candidate)
      if result == nil then result = true end
      package.loaded[name] = result
      return result
    end
    tried[#tried + 1] = candidate
  end
  error("module '" .. name .. "' not found:\n  " .. table.concat(tried, "\n  "), 2)
end
local timers = {}
function sys.after(ms, fn)
  local t = { at = sys.ticks() + ms, fn = fn }
  timers[#timers + 1] = t
  return t
end
function sys.every(ms, fn)
  local t = { at = sys.ticks() + ms, every = math.max(ms, 10), fn = fn }
  timers[#timers + 1] = t
  return t
end
function sys.cancel(t) t.cancelled = true end
local dialogs = {}
-- The "Open" and "Save as" dialogs: fn(path) is called when a file is chosen
-- (on cancel — fn(nil)). The program keeps running meanwhile.
function sys.open_file(fn, dir) dialogs[sys._dialog("open", dir or "", "")] = fn end
-- Several files at once: fn({paths}) or fn(nil) on cancel.
function sys.open_files(fn, dir)
  dialogs[sys._dialog("open_many", dir or "", "")] = function(text)
    if not text then return fn(nil) end
    local list = {}
    for p in text:gmatch("[^\n]+") do list[#list + 1] = p end
    fn(list)
  end
end
function sys.save_file(fn, name, dir) dialogs[sys._dialog("save", dir or "", name or "")] = fn end
-- The network: net.fetch(url) -> body, status (or nil, error). In a windowed program
-- the answer comes to a function: net.fetch(url, function(body, status) ... end).
local fetches, in_window = {}, false
function net.fetch(url, fn)
  if not in_window then
    local body, status = net._get(url)
    if fn then fn(body, status) return end
    return body, status
  end
  if not fn then error("in a window program use net.fetch(url, function(body, status) ... end)", 2) end
  fetches[net._start(url)] = fn
end
function ui.canvas(height, fn)
  local clicked, x, y, held, w, h, wheel = ui._canvas(height)
  if fn then
    gfx.w, gfx.h = w, h
    fn(gfx)
  end
  ui._canvas_end()
  return clicked, x, y, held, wheel
end
-- A canvas over the whole window, without margins: the program draws its UI itself.
-- fn(gfx, clicked, x, y, held, wheel).
function ui.fill(fn)
  local clicked, x, y, held, w, h, wheel = ui._canvas(0, true)
  gfx.w, gfx.h = w, h
  if fn then fn(gfx, clicked, x, y, held, wheel) end
  ui._canvas_end()
  return clicked, x, y, held, wheel
end
local function run_timers()
  if #timers == 0 then return false end
  local now, due = sys.ticks(), {}
  for i = #timers, 1, -1 do
    local t = timers[i]
    if t.cancelled then
      table.remove(timers, i)
    elseif now >= t.at then
      due[#due + 1] = t
      if t.every then t.at = now + t.every else table.remove(timers, i) end
    end
  end
  for i = #due, 1, -1 do due[i].fn() end
  return #due > 0
end
-- The loop of a windowed program: events from the kernel, timers, app.tick, frames.
local function app_loop()
  local redraw, next_tick = true, 0
  while true do
    local now = sys.ticks()
    local wait = redraw and 0 or 1000
    if app.tick then wait = math.min(wait, math.max(next_tick - now, 0)) end
    for i = 1, #timers do
      if not timers[i].cancelled then wait = math.min(wait, math.max(timers[i].at - now, 0)) end
    end
    local kind, a, b, c = ui._wait(wait)
    while kind do
      if kind == "redraw" then
        redraw = true
      elseif kind == "key" then
        if app.key and app.key(a) then redraw = true end
      elseif kind == "open" then
        if app.open then app.open(a) end
        redraw = true
      elseif kind == "dialog" then
        local fn = dialogs[a]
        dialogs[a] = nil
        if fn then fn(b) end
        redraw = true
      elseif kind == "fetch" then
        local fn = fetches[a]
        fetches[a] = nil
        if fn then fn(b, c) end
        redraw = true
      end
      kind, a, b, c = ui._wait(0)
    end
    if run_timers() then redraw = true end
    now = sys.ticks()
    -- app.tick returned a rectangle {x, y, w, h} — only this place changed:
    -- a partial frame, app.ui sees it in ui.area and may draw only it.
    local area
    if app.tick and now >= next_tick then
      next_tick = now + math.max(app.tick_every or 50, 10)
      local r = app.tick(ui)
      if r == true then redraw = true elseif type(r) == "table" then area = r end
    end
    if redraw then
      if ui._begin() then
        app.ui(ui)
        redraw = ui._end(collectgarbage("count") // 1)
      end
    elseif area then
      local ok, part = ui._begin(area[1], area[2], area[3], area[4])
      if ok then
        ui.area = part and area or nil
        app.ui(ui)
        ui.area = nil
        redraw = ui._end(collectgarbage("count") // 1)
      end
    end
  end
end
-- A program without a window that still has timers (for example, a service): wait for them.
local function timer_loop()
  while #timers > 0 do
    local now, wait = sys.ticks(), 1000
    for _, t in ipairs(timers) do wait = math.min(wait, math.max(t.at - now, 0)) end
    sys.sleep(wait)
    run_timers()
  end
end
function sys._start_app(name)
  if type(app) ~= "table" or type(app.ui) ~= "function" then return timer_loop() end
  ui._open(app.title or name, app.width or 480, app.height or 400, app.background, app.open ~= nil, app.min_width or 0, app.min_height or 0)
  in_window = true
  local ok, err = pcall(app_loop)
  ui._fail(tostring(err))
  while true do ui._wait(1000) end
end
-- Interactive mode: lines come from the terminal.
function sys._repl()
  while true do
    local line = sys._readline()
    if not line then return end
    local chunk, err = load("return " .. line, "=stdin")
    if not chunk then chunk, err = load(line, "=stdin") end
    if not chunk then
      print(err)
    else
      local r = table.pack(pcall(chunk))
      if not r[1] then
        print(r[2])
      elseif r.n > 1 then
        local out = {}
        for i = 2, r.n do out[i - 1] = tostring(r[i]) end
        print(table.concat(out, "   "))
      end
    end
  end
end
"#;

unsafe fn text_at(l: *mut lua_State, idx: c_int) -> Vec<u8> {
    let mut len = 0;
    let p = unsafe { lua_tolstring(l, idx, &mut len) };
    if p.is_null() { Vec::new() } else { unsafe { core::slice::from_raw_parts(p as *const u8, len) }.to_vec() }
}

/// Call arguments → values. Numbers become integers (fractions are rounded down).
unsafe fn take_args(l: *mut lua_State) -> Vec<Value> {
    let top = unsafe { lua_gettop(l) };
    (1..=top)
        .map(|i| unsafe {
            match lua_type(l, i) {
                LUA_TNIL => Value::Nil,
                LUA_TBOOLEAN => Value::Bool(lua_toboolean(l, i) != 0),
                LUA_TNUMBER => Value::Int(myos_toint(l, i, 0)),
                LUA_TSTRING => Value::Bytes(text_at(l, i)),
                LUA_TTABLE => {
                    let n = lua_rawlen(l, i) as i64;
                    let mut list = Vec::with_capacity(n as usize);
                    for k in 1..=n {
                        lua_rawgeti(l, i, k);
                        let mut len = 0;
                        let p = luaL_tolstring(l, -1, &mut len);
                        list.push(String::from_utf8_lossy(core::slice::from_raw_parts(p as *const u8, len)).into_owned());
                        lua_settop(l, -3);
                    }
                    Value::List(list)
                }
                _ => Value::Nil,
            }
        })
        .collect()
}

unsafe fn push_value(l: *mut lua_State, v: &Value) {
    unsafe {
        match v {
            Value::Nil => lua_pushnil(l),
            Value::Bool(b) => lua_pushboolean(l, *b as c_int),
            Value::Int(n) => lua_pushinteger(l, *n),
            Value::Bytes(b) => {
                lua_pushlstring(l, b.as_ptr() as *const c_char, b.len());
            }
            Value::List(items) => {
                lua_createtable(l, items.len() as c_int, 0);
                for (i, s) in items.iter().enumerate() {
                    lua_pushlstring(l, s.as_ptr() as *const c_char, s.len());
                    lua_rawseti(l, -2, i as i64 + 1);
                }
            }
        }
    }
}

/// An API function: the operation number is in an upvalue. The kernel's answer becomes Lua values.
unsafe extern "C" fn call(l: *mut lua_State) -> c_int {
    let op = unsafe { myos_toint(l, UPVALUE, 0) } as u64;
    let args = encode(&unsafe { take_args(l) });
    let mut reply = Vec::with_capacity(SHORT_REPLY);
    if !glass::lua_op(op, &args, &mut reply, SHORT_REPLY) {
        return 0;
    }
    let values = decode(&reply).unwrap_or_default();
    for v in &values {
        unsafe { push_value(l, v) };
    }
    values.len() as c_int
}

/// The fs, sys, net, ui, gfx tables — from the operation list.
unsafe fn register(l: *mut lua_State) {
    for lib in ["fs", "sys", "net", "ui", "gfx", "audio"] {
        let name = format!("{lib}\0");
        unsafe {
            lua_createtable(l, 0, 16);
            for (i, (table, func)) in OPS.iter().enumerate() {
                if *table == lib {
                    let func = format!("{func}\0");
                    lua_pushinteger(l, i as i64);
                    lua_pushcclosure(l, call, 1);
                    lua_setfield(l, -2, func.as_ptr() as *const c_char);
                }
            }
            lua_setglobal(l, name.as_ptr() as *const c_char);
        }
    }
}

/// Loads and runs a chunk of code. Err — the Lua error text.
unsafe fn exec(l: *mut lua_State, code: &[u8], name: &str) -> Result<(), String> {
    let chunk = format!("@{name}\0");
    unsafe {
        let mut status = luaL_loadbufferx(l, code.as_ptr() as *const c_char, code.len(), chunk.as_ptr() as *const c_char, core::ptr::null());
        if status == LUA_OK {
            status = lua_pcallk(l, 0, 0, 0, 0, None);
        }
        if status == LUA_OK {
            return Ok(());
        }
        let msg = String::from_utf8_lossy(&text_at(l, -1)).into_owned();
        lua_settop(l, -2);
        Err(msg)
    }
}

fn main(args: &[&str]) -> i32 {
    let l = unsafe { myos_newstate(MEMORY_LIMIT) };
    if l.is_null() {
        glass::println!("lua: out of memory");
        return 1;
    }
    unsafe {
        myos_openlibs(l);
        register(l);
    }
    if let Err(e) = unsafe { exec(l, PRELUDE.as_bytes(), "=prelude") } {
        glass::println!("lua: {e}");
        return 1;
    }
    let Some(&path) = args.get(1) else {
        return match unsafe { exec(l, b"sys._repl()", "=repl") } {
            Ok(()) => 0,
            Err(_) => 1,
        };
    };
    unsafe {
        lua_createtable(l, args.len() as c_int, 1);
        for (i, a) in args[1..].iter().enumerate() {
            lua_pushlstring(l, a.as_ptr() as *const c_char, a.len());
            lua_rawseti(l, -2, i as i64);
        }
        lua_setglobal(l, c"arg".as_ptr());
    }
    let Some(code) = glass::read_file(path) else {
        glass::println!("{path}: cannot read");
        return 1;
    };
    if let Err(e) = unsafe { exec(l, &code, path) } {
        glass::println!("{e}");
        return 1;
    }
    let name = path.rsplit('/').next().unwrap_or(path).trim_end_matches(".lua");
    let start = format!("sys._start_app({name:?})");
    match unsafe { exec(l, start.as_bytes(), "=app") } {
        Ok(()) => 0,
        Err(e) => {
            glass::println!("{e}");
            1
        }
    }
}

// ---------------------------------------------------------------- for libc

#[unsafe(no_mangle)]
extern "C" fn myos_alloc(size: usize) -> *mut u8 {
    match core::alloc::Layout::from_size_align(size, 16) {
        Ok(layout) => unsafe { alloc::alloc::alloc(layout) },
        Err(_) => core::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
extern "C" fn myos_free(ptr: *mut u8, size: usize) {
    if let Ok(layout) = core::alloc::Layout::from_size_align(size, 16) {
        unsafe { alloc::alloc::dealloc(ptr, layout) }
    }
}

#[unsafe(no_mangle)]
extern "C" fn myos_write(s: *const u8, n: usize) {
    glass::write(unsafe { core::slice::from_raw_parts(s, n) });
}

#[unsafe(no_mangle)]
extern "C" fn myos_abort(msg: *const c_char) -> ! {
    let text = unsafe { core::ffi::CStr::from_ptr(msg) }.to_string_lossy();
    glass::println!("lua: {text}");
    glass::exit(1)
}

#[unsafe(no_mangle)]
extern "C" fn myos_ticks_ms() -> u64 {
    glass::ticks()
}
