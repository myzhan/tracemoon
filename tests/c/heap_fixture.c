/* Build against Lua 5.4; --dump writes allocator-owned regions for portable decoder tests.
 * Without arguments, stay alive for a real Linux ptrace capture.
 */
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include "lua.h"
#include "lauxlib.h"
#include "lualib.h"

typedef struct { void *pointer; size_t size; } Block;
static Block blocks[100000];
static size_t count;
static void *tracking_alloc(void *ud, void *p, size_t old, size_t size) {
    size_t i;
    (void)ud; (void)old;
    for (i = 0; i < count && blocks[i].pointer != p; ++i) {}
    if (size == 0) {
        free(p);
        if (i < count) blocks[i] = blocks[--count];
        return NULL;
    }
    void *next = realloc(p, size);
    if (!next) return NULL;
    if (i == count) {
        if (count == 100000) abort();
        ++count;
    }
    blocks[i] = (Block){ next, size };
    return next;
}
static void word(FILE *f, uint64_t value) {
    if (fwrite(&value, sizeof(value), 1, f) != 1) abort();
}

int main(int argc, char **argv) {
    lua_State *L = lua_newstate(tracking_alloc, NULL);
    if (!L) return 1;
    luaL_openlibs(L);
    lua_gc(L, LUA_GCSTOP);
    const char *script =
        "local sessions = {}\n"
        "for i=1,200 do sessions['uid:'..i]={payload=string.rep('x',1000)..i} end\n"
        "local function flush() return sessions end\n"
        "package.loaded['player.cache']={sessions=sessions,flush=flush}\n"
        "package.loaded['alias']={sessions=sessions}\n"
        "sessions.self=sessions\n"
        "local weak=setmetatable({}, {__mode='v'})\n"
        "weak.lost={payload=string.rep('w',3000)}\n"
        "package.loaded['weak.fixture']=weak\n"
        "local eph=setmetatable({}, {__mode='k'})\n"
        "eph[{}]={secret='conditional'}\n"
        "package.loaded['ephemeron.fixture']=eph\n"
        "local co=coroutine.create(function() local captured={}; coroutine.yield(function() return captured end) end)\n"
        "coroutine.resume(co)\n"
        "package.loaded['coroutine.fixture']=co\n"
        "local dead=coroutine.create(function() end)\n"
        "local finalized=setmetatable({}, {__gc=function() end})\n"
        "package.loaded['finalized.fixture']=finalized\n";
    if (luaL_loadbuffer(L, script, strlen(script), "@heap_fixture.lua") || lua_pcall(L, 0, 0, 0)) {
        fprintf(stderr, "%s\n", lua_tostring(L, -1)); return 2;
    }
    lua_newuserdatauv(L, 4096, 1);
    lua_getglobal(L, "package");
    lua_setiuservalue(L, -2, 1);
    lua_setfield(L, LUA_REGISTRYINDEX, "fixture.userdata");
    if (argc == 3 && strcmp(argv[1], "--dump") == 0) {
        FILE *f = fopen(argv[2], "wb");
        if (!f) return 3;
        word(f, (uintptr_t)L);
        word(f, count);
        for (size_t i = 0; i < count; ++i) {
            word(f, (uintptr_t)blocks[i].pointer);
            word(f, blocks[i].size);
            if (fwrite(blocks[i].pointer, blocks[i].size, 1, f) != 1) abort();
        }
        fclose(f);
        lua_close(L);
        return 0;
    }
    printf("READY lua_State=0x%llx\n", (unsigned long long)(uintptr_t)L);
    fflush(stdout);
    for (;;) sleep(1);
}
