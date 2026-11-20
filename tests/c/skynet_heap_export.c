/* Test-only cooperative heap export for a real snlua service on macOS/Linux.
 * Called from that VM's owner thread at a Lua C-call boundary. This is NOT the
 * external ptrace capture path and is never linked into tracemoon or upstreams.
 */
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>
#include <unistd.h>
#include <sys/stat.h>
#include "lua.h"
#include "lauxlib.h"
#include "lstate.h"
#include "lfunc.h"
#include "lgc.h"
#include "ltable.h"

typedef struct {
    FILE *file; uint64_t count; int failed;
    GCObject **shared; size_t shared_count, shared_capacity;
} Writer;

static void object(Writer *w, GCObject *o);

static void shared(Writer *w, GCObject *o) {
    if (!o || !isshared(o) || w->failed) return;
    for (size_t i = 0; i < w->shared_count; ++i) if (w->shared[i] == o) return;
    if (w->shared_count == w->shared_capacity) {
        size_t capacity = w->shared_capacity ? w->shared_capacity * 2 : 256;
        if (capacity > 100000) { w->failed = 1; return; }
        GCObject **next = realloc(w->shared, capacity * sizeof(*next));
        if (!next) { w->failed = 1; return; }
        w->shared = next; w->shared_capacity = capacity;
    }
    w->shared[w->shared_count++] = o;
    object(w, o);
}

static void shared_value(Writer *w, const TValue *v) {
    if (iscollectable(v)) shared(w, gcvalue(v));
}

static void word(Writer *w, uint64_t n) {
    if (fwrite(&n, sizeof(n), 1, w->file) != 1) w->failed = 1;
}

static void region(Writer *w, const void *p, size_t size) {
    if (p == NULL || size == 0 || w->failed) return;
    word(w, (uintptr_t)p);
    word(w, size);
    if (fwrite(p, size, 1, w->file) != 1) w->failed = 1;
    ++w->count;
}

static void object(Writer *w, GCObject *o) {
    switch (o->tt) {
    case LUA_VSHRSTR:
    case LUA_VLNGSTR: {
        TString *s = gco2ts(o);
        region(w, s, offsetof(TString, contents) + tsslen(s) + 1);
        break;
    }
    case LUA_VTABLE: {
        Table *t = gco2t(o);
        unsigned int n = t->alimit;
        if (!isrealasize(t) && n > 0) {
            --n; n |= n >> 1; n |= n >> 2; n |= n >> 4;
            n |= n >> 8; n |= n >> 16; ++n;
        }
        region(w, t, sizeof(*t));
        region(w, t->array, (size_t)n * sizeof(TValue));
        if (t->lastfree) region(w, t->node, ((size_t)1 << t->lsizenode) * sizeof(Node));
        for (size_t i = 0; i < n; ++i) shared_value(w, &t->array[i]);
        if (t->lastfree) {
            for (size_t i = 0; i < ((size_t)1 << t->lsizenode); ++i) {
                Node *node = &t->node[i];
                if (isempty(gval(node))) continue;
                shared_value(w, gval(node));
                if (keyiscollectable(node)) shared(w, gckey(node));
            }
        }
        break;
    }
    case LUA_VLCL: {
        LClosure *c = gco2lcl(o);
        region(w, c, sizeLclosure(c->nupvalues));
        shared(w, (GCObject *)c->p);
        break;
    }
    case LUA_VCCL: {
        CClosure *c = gco2ccl(o);
        region(w, c, sizeCclosure(c->nupvalues));
        for (size_t i = 0; i < c->nupvalues; ++i) shared_value(w, &c->upvalue[i]);
        break;
    }
    case LUA_VUPVAL:
        region(w, o, sizeof(UpVal));
        shared_value(w, ((UpVal *)o)->v.p);
        break;
    case LUA_VUSERDATA: {
        Udata *u = gco2u(o);
        region(w, u, sizeudata(u->nuvalue, u->len));
        for (size_t i = 0; i < u->nuvalue; ++i) shared_value(w, &u->uv[i].uv);
        break;
    }
    case LUA_VTHREAD: {
        lua_State *th = gco2th(o);
        region(w, th, sizeof(*th));
        if (th->stack.p) region(w, th->stack.p,
            (th->stack_last.p - th->stack.p + EXTRA_STACK) * sizeof(StackValue));
        for (StkId slot = th->stack.p; slot && slot < th->top.p; ++slot) shared_value(w, s2v(slot));
        break;
    }
    case LUA_VPROTO: {
        Proto *p = gco2p(o);
        region(w, p, sizeof(*p));
        region(w, p->k, (size_t)p->sizek * sizeof(TValue));
        region(w, p->code, (size_t)p->sizecode * sizeof(Instruction));
        region(w, p->p, (size_t)p->sizep * sizeof(Proto *));
        region(w, p->upvalues, (size_t)p->sizeupvalues * sizeof(Upvaldesc));
        region(w, p->lineinfo, (size_t)p->sizelineinfo);
        region(w, p->abslineinfo, (size_t)p->sizeabslineinfo * sizeof(AbsLineInfo));
        region(w, p->locvars, (size_t)p->sizelocvars * sizeof(LocVar));
        shared(w, (GCObject *)p->source);
        for (int i = 0; i < p->sizek; ++i) shared_value(w, &p->k[i]);
        for (int i = 0; i < p->sizep; ++i) shared(w, (GCObject *)p->p[i]);
        for (int i = 0; i < p->sizeupvalues; ++i) shared(w, (GCObject *)p->upvalues[i].name);
        for (int i = 0; i < p->sizelocvars; ++i) shared(w, (GCObject *)p->locvars[i].varname);
        break;
    }
    default: w->failed = 1;
    }
}

static uint64_t monotonic_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000000000 + ts.tv_nsec;
}

static int dump(lua_State *L) {
    const char *path = luaL_checkstring(L, 1);
    uint64_t handle = (uint64_t)luaL_checkinteger(L, 2);
    uint64_t started = monotonic_ns();
    Writer w = {0};
    w.file = fopen(path, "wb");
    if (!w.file) return luaL_error(L, "cannot open heap export");
    fchmod(fileno(w.file), 0600);
    word(&w, (uintptr_t)L);
    word(&w, 0); /* region count patched after copying */
    global_State *g = G(L);
    region(&w, g, sizeof(*g));
    GCObject *lists[] = {g->allgc, g->finobj, g->tobefnz, g->fixedgc};
    size_t visited = 0;
    for (size_t i = 0; i < sizeof(lists) / sizeof(lists[0]); ++i) {
        for (GCObject *o = lists[i]; o != NULL && !w.failed; o = o->next) {
            if (++visited > 1000000) { w.failed = 1; break; }
            object(&w, o);
        }
    }
    word(&w, (uint64_t)getpid());
    word(&w, (uint64_t)time(NULL));
    word(&w, monotonic_ns() - started);
    word(&w, handle);
    if (fseek(w.file, sizeof(uint64_t), SEEK_SET) != 0) w.failed = 1;
    word(&w, w.count);
    if (fclose(w.file) != 0) w.failed = 1;
    free(w.shared);
    if (w.failed) return luaL_error(L, "incomplete cooperative heap export");
    return 0;
}

int luaopen_tracemoon_heap_export(lua_State *L) {
    luaL_Reg functions[] = {{"dump", dump}, {NULL, NULL}};
    luaL_newlib(L, functions);
    return 1;
}
