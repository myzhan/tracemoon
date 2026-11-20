/* Test-only Lua entry point: stop GDB after all business fixtures are initialized.
 * No Lua ABI fields or API calls are needed, so both vendors can load this module.
 */
#include <signal.h>
struct lua_State;
int luaopen_tracemoon_core_trigger(struct lua_State *L) {
    (void)L;
    raise(SIGABRT);
    return 0;
}
