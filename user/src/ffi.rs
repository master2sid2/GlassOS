//! Declarations of the Lua 5.4 C API and our glue from libc/src/glue.c.
//!
//! Important: the Rust part uses the soft-float ABI, the C part ordinary SSE. So there is
//! not a single function with a double in its signature here; numbers are passed as integers
//! (`lua_pushinteger`, `myos_toint`).

#![allow(non_camel_case_types)]

use core::ffi::{c_char, c_int};

pub enum lua_State {}

pub type lua_CFunction = unsafe extern "C" fn(*mut lua_State) -> c_int;

pub const LUA_OK: c_int = 0;
pub const LUA_MULTRET: c_int = -1;
pub const LUA_TTABLE: c_int = 5;
pub const LUA_TFUNCTION: c_int = 6;

unsafe extern "C" {
    pub fn lua_gettop(l: *mut lua_State) -> c_int;
    pub fn lua_settop(l: *mut lua_State, idx: c_int);
    pub fn lua_type(l: *mut lua_State, idx: c_int) -> c_int;
    pub fn lua_toboolean(l: *mut lua_State, idx: c_int) -> c_int;
    pub fn lua_tolstring(l: *mut lua_State, idx: c_int, len: *mut usize) -> *const c_char;
    pub fn lua_rawlen(l: *mut lua_State, idx: c_int) -> u64;
    pub fn lua_pushnil(l: *mut lua_State);
    pub fn lua_pushinteger(l: *mut lua_State, n: i64);
    pub fn lua_pushlstring(l: *mut lua_State, s: *const c_char, len: usize) -> *const c_char;
    pub fn lua_pushboolean(l: *mut lua_State, b: c_int);
    pub fn lua_pushcclosure(l: *mut lua_State, f: lua_CFunction, n: c_int);
    pub fn lua_createtable(l: *mut lua_State, narr: c_int, nrec: c_int);
    pub fn lua_getglobal(l: *mut lua_State, name: *const c_char) -> c_int;
    pub fn lua_setglobal(l: *mut lua_State, name: *const c_char);
    pub fn lua_getfield(l: *mut lua_State, idx: c_int, k: *const c_char) -> c_int;
    pub fn lua_setfield(l: *mut lua_State, idx: c_int, k: *const c_char);
    pub fn lua_rawgeti(l: *mut lua_State, idx: c_int, n: i64) -> c_int;
    pub fn lua_rawseti(l: *mut lua_State, idx: c_int, n: i64);
    pub fn lua_pushvalue(l: *mut lua_State, idx: c_int);
    pub fn lua_callk(
        l: *mut lua_State,
        nargs: c_int,
        nresults: c_int,
        ctx: isize,
        k: Option<unsafe extern "C" fn(*mut lua_State, c_int, isize) -> c_int>,
    );
    pub fn luaL_tolstring(l: *mut lua_State, idx: c_int, len: *mut usize) -> *const c_char;
    pub fn lua_pcallk(
        l: *mut lua_State,
        nargs: c_int,
        nresults: c_int,
        errfunc: c_int,
        ctx: isize,
        k: Option<unsafe extern "C" fn(*mut lua_State, c_int, isize) -> c_int>,
    ) -> c_int;
    pub fn luaL_loadbufferx(
        l: *mut lua_State,
        buf: *const c_char,
        size: usize,
        name: *const c_char,
        mode: *const c_char,
    ) -> c_int;

    pub fn myos_newstate(limit: usize) -> *mut lua_State;
    pub fn myos_close(l: *mut lua_State);
    pub fn myos_memused(l: *mut lua_State) -> usize;
    pub fn myos_openlibs(l: *mut lua_State);
    pub fn myos_toint(l: *mut lua_State, idx: c_int, def: i64) -> i64;
}

pub unsafe fn lua_pop(l: *mut lua_State, n: c_int) {
    unsafe { lua_settop(l, -n - 1) }
}
