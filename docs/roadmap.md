# Roadmap

Where tracemoon is, and what we intend to build next.

`docs/lua-profiler-ptrace.md` describes what exists today and why it is shaped that way.
This file describes what is *missing* and why we would build it. It is a set of design
notes, not a promise: ordering inside a tier is a cost/benefit judgment, and every item is
written so that whoever implements it does not have to re-derive the reasoning.

## Where we are

The **CPU hotspot** axis is done: Lua `CallInfo` walking for 5.4/5.5, native C merge on
Linux x86_64 and macOS, skynet multi-VM, symbol resolution, and three output modes
(`record` / `dump` / `top`).

Two axes are untouched:

- **Time.** The aggregator answers "how often was this function on the stack" but not
  "how long did this stay on the stack". For a service written against skynet, the
  interesting failure is rarely a hot function — it is one message that took 340 ms.
- **Memory.** A sampling profiler that reads `global_State` can see *every* live GC object.
  That is a completely different question from "where is the CPU", and today tracemoon
  cannot answer it at all.

## How to read the tiers

- **Tier 1** — self-contained, a day or less each, no new unsafe code, no new layout tables.
- **Tier 2** — shaped by the skynet use case; touches the aggregator's data model or the
  first TValue *value* decoding.
- **Tier 3** — strategic: a new subsystem or a second Lua implementation.

Every item states **Why**, **How** (the modules it lands in and the invariants it must not
break), **Cost**, and **Risk**.

---

## Tier 1 — cheap and self-contained

### T1-1 Stack residency (long-tail stalls)

**Why.** A sampling profiler's unique advantage over instrumentation is time, and we throw
it away: `Aggregator` keeps counts only. In skynet, a service that blocks for 300 ms on one
message contributes 30 samples out of 30 000 — invisible in the flamegraph, and exactly the
thing an operator cares about.

**How.** Track consecutive runs of the same stack in `src/output/aggregator.rs`:

- `HashMap<stack_key, Streak { started_at: Instant, ticks: u64 }>`.
- In `add_tick`, every key seen this tick extends its streak; every *unseen* key settles —
  if `ticks >= threshold` it becomes a candidate for a top-N longest-stall list.
- Report `Longest stalls:` with duration, the tick offset it started at, and the full stack.

**Invariants.** The streak counter must be keyed on the **stack**, not on the tick. In
skynet mode one tick yields one stack *per service*, so a tick-keyed counter would collapse
"three services stalled simultaneously" into a single stall and report the wrong duration.
This is the same distinction that `add_tick` vs `sampled_stacks()` already enforces for the
success rate.

**Cost.** ~100–150 lines plus a test that feeds a synthetic tick sequence (fast tick, long
stall, fast tick) and asserts the reported duration. `--stall-threshold-ms` (default 100)
keeps it configurable by time rather than by tick count, so it survives `-r` changes.

**Risk.** Low. No remote reads, no layout, no unsafe.

### T1-2 `--format speedscope`

**Why.** The SVG only shows the *aggregate*, and it cannot be diffed. Speedscope
(`https://www.speedscope.app`) is the de-facto sampled-profile interchange format: Firefox,
Perfetto and the VS Code profiler all open it, it shows the sample timeline, and it compares
two files side by side. That is how you answer "what changed between these two runs".

**How.** New `src/output/speedscope.rs` emitting a `SampledFile`:

- `shared.frames` — one entry per distinct frame name, interned once.
- `profiles[0] = { type: "sampled", unit: "seconds", samples: [[frame_ix…]], weights: [1/rate…] }`.

**Invariants.**
- `Aggregator` currently keeps only counts, so this needs an opt-in **per-tick stack
  sequence**. Store frame indices into an interned table rather than strings
  (`Vec<Vec<u32>>`), and only when the output format asks for it — an hour at 100 Hz with
  100 frames deep is ~1.4 M indices, acceptable, but only if interning is in place.
- Speedscope samples are ordered **root → leaf**, while `StackWalker` yields
  deepest-first and the aggregator reverses. Re-reverse here explicitly; getting this wrong
  produces an upside-down (but plausible-looking) profile.

**Cost.** ~150–200 lines. Main design decision is the per-tick buffer and its memory cap
(drop-and-warn past `--max-samples` rather than OOM).

**Risk.** Low-medium: the buffer changes `Aggregator`'s shape, so `dump`/`top` call sites
need a look.

### T1-3 Profile sidecar metadata

**Why.** A flamegraph file detached from its context is not comparable: no pid, no command
line, no sample rate, no Lua version, no success rate. Archiving and regression-diffing both
need it.

**How.** Write `<output>.meta.json` next to the profile:

```json
{ "tracemoon": "0.1.0", "pid": 1234, "command": ["lua", "app.lua"], "lua_version": "5.4",
  "sample_rate": 100, "duration_secs": 30.0, "ticks": 2996, "success_rate": 99.5,
  "started_at": "2026-11-20T10:00:00Z" }
```

**Invariant.** Do not add `serde` for six scalars — a 20-line escaper over already-typed
values keeps the dependency table clean (`AGENTS.md` already treats unused/extra deps as a
smell). Skip the sidecar when the output goes to stdout.

**Cost.** ~50–80 lines in `collapsed.rs`/`sampler.rs`.

### T1-4 Durable runs (checkpoint + error-path flush)

**Why.** Today `run_profiler` is `sampler.run()?; sampler.generate_output()?;` — if `run`
returns `Err` (a resolver failure at the wrong moment, an IO error), every sample collected
so far is discarded. A `-d 0` run that is OOM-killed or reboots loses the whole session.

Note what is *not* missing: `ctrlc::set_handler` already covers SIGINT, SIGTERM and SIGHUP,
so a plain `kill` does exit through the normal path. The gap is mid-run durability and the
error path.

**How.**
1. Error paths still flush: `let result = sampler.run(); sampler.generate_output(); result?;`
   in all three entry points.
2. `--checkpoint <PATH> --checkpoint-interval <SECS>`: every N seconds write `PATH.tmp` then
   `rename(2)`, so a reader never observes a half-written file.

**Invariant.** Never write on every tick — the sample rate is user-controlled; an
interval-based checkpoint is the only version that cannot turn a profiler into an IO storm.

**Cost.** ~80 lines.

### T1-5 GC share in the statistics

**Why.** Tuning `collectgarbage("setpause")`/`setstepmul` is guesswork without knowing what
fraction of samples the collector owns.

**How.** In `add_tick`, if any frame name starts with `luaC_`, `lua_gc` or `luaE_`, count the
tick as a GC tick. Print `GC ticks: 12.3%` in the stats block.

**Invariant.** Report **unknown**, not 0%, when the symbols could not be resolved (a
stripped interpreter shows `[C]:lua5.4+0x…`). Silently reporting "no GC" for an unresolvable
binary is worse than reporting nothing.

**Cost.** ~40 lines, pure string matching. Note that C frame names come from the resolver, so
this is only meaningful once symbols are available — check the smoke output for the shape.

---

## Tier 2 — skynet-shaped

### T2-1 Per-service grouping and filtering

**Why.** A skynet process with 100 services produces 100 `[service:name]` roots in one
flamegraph; the wide-but-flat result is unreadable, and the operator usually wants one
service or a per-service breakdown.

**How.** `SkynetDiscovery` already yields the service name per VM (`src/process/skynet.rs`).
Tag each stack at sample time and bucket the aggregator per service
(`HashMap<Option<String>, Aggregator>`). CLI surface:

- `--service <name>` (repeatable) — profile only those services.
- `--split-by-service` — one output file per service (`out.<service>.svg`).
- `--group-by service` — keep the single file but root by service (today's behaviour).

**Cost.** ~150–200 lines. Risk is the aggregator balkanisation rippling into `dump`/`top`;
consider making the bucket key part of the existing struct rather than wrapping aggregators
everywhere.

### T2-2 Idle / blocking attribution

**Why.** An idle skynet service sits in `epoll_wait`; nearly every sample lands there, it
tops every hotspot chart, and it hides the real work. "Waiting" and "working" must be
separated before the ranking means anything.

**How.** A known-blocking symbol table (`epoll_wait`, `epoll_pwait`, `poll`, `select`,
`nanosleep`, `pthread_cond_wait`, `futex`, `accept`, `recv*`, `read`) plus a per-tick check:
if the innermost **C** frame matches, the tick is idle. `--idle` includes idle samples in the
profile (py-spy's flag); the default excludes them and the stats block reports the idle share.

**Invariant.** Test the innermost *C* frame, not the last frame in the merged stack — the
aggregator appends the native chain *after* the Lua frames, so "last" is the process entry
point, not where the thread is parked.

**Cost.** ~100 lines, string matching only, no layout risk.

### T2-3 `dump --locals`

**Why.** `dump` shows the stack but not the values. "Why did it take this branch" is usually
answered by one variable.

**How.** For each Lua frame, read `Proto.locvars` (`LocVar { varname: TString*, startpc,
endpc }`) and print every variable with `startpc <= currentpc < endpc`, decoding the value
from the frame's register window (`ci.func + 1 …`). Handle varargs via `ci.u.l.nextraargs`.

**Risk.** This is the first time the crate decodes a TValue's *value* (today it reads the tag
only, to pick an address). It needs a value formatter in `layout.rs` plus honest failure:
torn reads produce garbage, so any value that does not type-check must print `?` rather than
something plausible.

**Cost.** ~200–300 lines. Highest risk in this tier.

---

## Tier 3 — strategic

### T3-1 Memory object sampling (`tracemoon mem`)

**Why.** The highest-value item after CPU profiling. Walking `global_State.allgc` yields
*every* live GC object, so we can answer "which Lua file's tables are holding 800 MB" — the
canonical Lua service leak question. It reuses the whole existing skeleton: external reads,
`LuaLayout` for version drift, the defensive-read discipline.

**How.**
1. Add a `global_State` offset table to `LuaLayout` (`allgc`, `totalbytes`, `GCdebt`),
   cross-checked with `#[repr(C)]` + `offset_of!` exactly like the 5.4 `lua_State` table in
   `types.rs`.
2. Walk `allgc.next`, dispatching on the object header's `tt` (note the header differs
   between 5.4 and 5.5 — parametrize it, do not hardcode 5.4's field order).
3. Size per type: `TString` = header + len (5.5 long strings go through `contents`);
   `Table` = struct + `sizearray * 16` + node array; `Proto` = struct + each array;
   `LClosure`/`CClosure` = struct + `nupvalues * 16`; `Udata` = struct + `len`.
4. Aggregate `Table`/`Proto` by `Proto.source` — that is what turns bytes into an answerable
   question ("md5.lua").

**Invariants.** The list is live and the collector mutates it while we read. Cap the walk
(e.g. 10 M objects), tolerate per-object failure, and bound the traversal so a corrupt `next`
pointer cannot loop forever — the same defensive posture as the `CallInfo` walk, with a
different loop-detection strategy (min visit count / monotonicity heuristic, because a
visited-set over millions of nodes is not affordable).

**Cost.** ~500 lines plus a `mem` subcommand. Highest product value in this document.

### T3-2 LuaJIT support

**Why.** It is the largest user population we do not serve.

**Why it is genuinely hard** (this is the part to read before committing to it):

- Different `lua_State` (`base`, `top`, `ci`, `cframe`, …) — a second layout, not a variant
  of the existing one.
- Frames live in `CallInfo` with `func`/`pc`/`savedpc`/`nresults`/`callstatus`, but there is
  no per-instruction `lineinfo` array: line numbers come from `Proto.lineinfo` indexed by
  `pc - Proto.code`, with a different encoding than 5.4.
- **JIT-compiled traces have no Lua frames.** Under `-O2` the interpreter is not running, so
  a large part of the stack is mcode with no `CallInfo` at all. Any implementation must say
  so in its output rather than presenting a truncated stack as the truth.

**Suggested shape.** A first-class `LuaVersion::Jit` + `lua_jit()` constructor (the reason
`LuaLayout` exists), scoped initially to accurate walks under `-joff`/`-O0`, then best-effort
attribution of mcode samples to enclosing traces. One to two weeks, and a separate release.

---

## Non-goals

- **Windows.** The audience runs Lua backends on Linux; `remoteprocess` and libunwind-ptrace
  do not target it.
- **Lua 5.1–5.3.** Structure differs as much as 5.5 does, and the installed base has moved
  to 5.4 or LuaJIT.
- **DWARF line numbers for C frames.** Production processes are stripped; resolving
  `[C]:lua5.4+0x7e55` honestly is better than shipping a lookup that mostly misses.

## Suggested sequencing

1. **T1-4** first — every later feature wants a run that survives long enough to be
   inspected, and the error-path flush is a five-line fix with a large payoff.
2. **T1-1** — half a day, and it changes what the tool can answer.
3. **T1-3 + T1-5** — same diff, both are small and touch the stats block together.
4. **T1-2** — needs the per-tick buffer, which also gives T1-1 its streak bookkeeping.
5. **T2-2** — makes the existing `top` ranking trustworthy before we add more views.
6. **T2-1** — the aggregator restructure that **T3-1** will also want.
7. **T3-1** (`mem`) — the first feature in a new dimension.
8. **T3-2** (LuaJIT) — its own release.

Dependencies worth noting: T1-2 and T1-1 share the per-tick record; T2-1's per-service bucket
key is also the natural place to attach T3-1's per-service memory totals.
