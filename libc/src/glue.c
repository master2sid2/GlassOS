/* Glue between Lua and /bin/lua: creating a state with a memory limit, the library set
 * and helper functions that need double. All the floating-point code
 * stays in C: the Rust part uses the soft-float ABI, so only integers and pointers
 * cross the Rust <-> C boundary. */

#include <math.h>
#include <stdlib.h>

#include "lauxlib.h"
#include "lua.h"
#include "lualib.h"

extern void myos_abort(const char *msg) __attribute__((noreturn));

typedef struct {
    size_t used, limit;
} memctl;

static void *lua_alloc(void *ud, void *ptr, size_t osize, size_t nsize) {
    memctl *m = ud;
    size_t old = ptr ? osize : 0;
    if (nsize == 0) {
        free(ptr);
        m->used -= old;
        return NULL;
    }
    if (m->used - old + nsize > m->limit) return NULL;
    void *p = realloc(ptr, nsize);
    if (p) m->used = m->used - old + nsize;
    return p;
}

static int on_panic(lua_State *L) {
    const char *msg = lua_tostring(L, -1);
    myos_abort(msg ? msg : "lua panic");
}

lua_State *myos_newstate(size_t limit) {
    memctl *m = malloc(sizeof *m);
    if (!m) return NULL;
    m->used = 0;
    m->limit = limit;
    lua_State *L = lua_newstate(lua_alloc, m);
    if (!L) {
        free(m);
        return NULL;
    }
    lua_atpanic(L, on_panic);
    return L;
}

void myos_close(lua_State *L) {
    void *ud;
    lua_getallocf(L, &ud);
    lua_close(L);
    free(ud);
}

size_t myos_memused(lua_State *L) {
    void *ud;
    lua_getallocf(L, &ud);
    return ((memctl *)ud)->used;
}

void myos_openlibs(lua_State *L) {
    static const luaL_Reg libs[] = {
        {LUA_GNAME, luaopen_base},          {LUA_COLIBNAME, luaopen_coroutine},
        {LUA_TABLIBNAME, luaopen_table},    {LUA_STRLIBNAME, luaopen_string},
        {LUA_MATHLIBNAME, luaopen_math},    {LUA_UTF8LIBNAME, luaopen_utf8},
        {NULL, NULL},
    };
    for (const luaL_Reg *lib = libs; lib->func; lib++) {
        luaL_requiref(L, lib->name, lib->func, 1);
        lua_pop(L, 1);
    }
    /* libc has no files — remove what would not work anyway. */
    lua_pushnil(L);
    lua_setglobal(L, "dofile");
    lua_pushnil(L);
    lua_setglobal(L, "loadfile");
}

/* The number at an index as an integer (fractions are rounded down); def — if it is not a number. */
long long myos_toint(lua_State *L, int idx, long long def) {
    int isnum;
    lua_Number n = lua_tonumberx(L, idx, &isnum);
    if (!isnum || n != n) return def;
    if (n >= 9.2e18) return 9200000000000000000LL;
    if (n <= -9.2e18) return -9200000000000000000LL;
    return (long long)floor(n);
}
