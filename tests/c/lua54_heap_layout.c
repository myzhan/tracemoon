/* Compile against an unmodified Lua 5.4 source tree:
 * cc -std=c11 -I/path/to/lua-5.4.7/src tests/c/lua54_heap_layout.c -o /tmp/layout
 * This verifies heap ABI independently of tracemoon's Rust reference structs.
 */
#include <stddef.h>
#include "lstate.h"
#include "lfunc.h"
#include "lgc.h"
#define OFF(T, f, n) _Static_assert(offsetof(T, f) == (n), #T "." #f)
#define SIZE(T, n) _Static_assert(sizeof(T) == (n), #T)
OFF(global_State, l_registry, 64);
OFF(global_State, allgc, 112);
OFF(global_State, finobj, 128);
OFF(global_State, tobefnz, 176);
OFF(global_State, fixedgc, 184);
OFF(global_State, mainthread, 264);
OFF(global_State, memerrmsg, 272);
OFF(global_State, tmname, 280);
OFF(global_State, mt, 480);
#ifdef TRACEMOON_SKYNET_LUA
_Static_assert(G_SHARED == 7 && AGEBITS == 7, "shared object age");
OFF(global_State, gcstate, 97);
#else
OFF(global_State, gcstate, 101);
#endif
OFF(lua_State, top, 16);
OFF(lua_State, l_G, 24);
OFF(lua_State, stack_last, 40);
OFF(lua_State, stack, 48);
OFF(lua_State, openupval, 56);
SIZE(lua_State, 200);
SIZE(Table, 56);
OFF(Table, flags, 10);
OFF(Table, lsizenode, 11);
OFF(Table, alimit, 12);
OFF(Table, array, 16);
OFF(Table, node, 24);
OFF(Table, lastfree, 32);
OFF(Table, metatable, 40);
SIZE(Node, 24);
OFF(Node, u.key_tt, 9);
OFF(Node, u.key_val, 16);
/* 5.4.7 includes contents[1] in sizeof; allocation uses offsetof(contents). */
#ifdef TRACEMOON_SKYNET_LUA
OFF(TString, contents, 32);
OFF(TString, u.lnglen, 24);
#else
OFF(TString, contents, 24);
OFF(TString, u.lnglen, 16);
#endif
SIZE(UpVal, 40);
OFF(UpVal, v, 16);
OFF(UpVal, u.value, 24);
OFF(LClosure, upvals, 32);
OFF(CClosure, upvalue, 32);
OFF(Udata, nuvalue, 10);
OFF(Udata, len, 16);
OFF(Udata, metatable, 24);
OFF(Udata, uv, 40);
_Static_assert(udatamemoffset(0) == 32, "userdata0");
_Static_assert(sizeof(UValue) == 16, "uservalue");
_Static_assert(LUA_VUPVAL == 9 && LUA_VPROTO == 10, "internal tags");
_Static_assert(TM_N == 25, "tag methods");
int main(void) { return 0; }
