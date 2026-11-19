# tracemoon implementation notes

How the profiler is put together, and *why* the non-obvious parts are the way they are.
The module-level doc comments in `src/` are the other half of this document; `AGENTS.md`
holds the short list of invariants.

**What it is:** a sampling profiler for Lua 5.4 / 5.5, modelled on
[py-spy](https://github.com/benfred/py-spy). It reads the target's memory from the outside —
no injection, no instrumentation — walks the Lua `CallInfo` chain (plus the native C stack
where that is meaningful), aggregates samples and emits a flamegraph SVG, collapsed stacks,
a one-shot snapshot, or a live `top` view. `skynet` processes, which host many Lua VMs in a
single OS process, are supported first-class.

**Targets:** plain Lua 5.4 / 5.5 (single VM) and skynet (multi VM).
**Platforms:** Linux x86_64 (primary), Linux aarch64, macOS x86_64 / arm64.

---

## Control flow

```
main.rs                 CLI dispatch: (pid, command) -> attach | spawn | error
 └─ config.rs           clap parsing -> Config { mode, pid, command, rate, duration, ... }
 └─ sampler.rs          Sampler: Target, reader, layout, Mode, resolver, aggregator
     ├─ Target          Attach(Pid) | Spawn(SpawnedChild)
     ├─ Mode            Single { addr: Cell } | Skynet { h_addr }
     ├─ run()           tick loop @ rate Hz; resumes a spawned child; stops on
     │                  duration / Ctrl+C / target exit
     └─ sample()        walk Lua stack(s) (+ native merge in single mode) -> aggregate
```

One tick produces **one stack** in single-VM mode and **one stack per service** in skynet
mode. Ticks and stacks are counted separately (see "Sampling model" below).

---

## Module map

```
src/
├── main.rs                 CLI entry point, permission warnings, error -> exit code
├── config.rs               clap definitions -> Config (also owns PID validation)
├── sampler.rs              Sampling loop, target lifecycle, output orchestration
├── error.rs                TracemoonError (thiserror)
│
├── lua/
│   ├── layout.rs           LuaLayout: all version-specific offsets + typed field readers
│   ├── types.rs            Lua 5.4 #[repr(C)] reference structs + tag helpers
│   └── stack.rs            StackWalker (CallInfo chain -> StackFrame) + frame formatting
│
├── process/
│   ├── mod.rs              Platform-agnostic API (reader factory, process_exists)
│   ├── reader.rs           ProcessMemoryReader trait, Pod reads, address validation
│   ├── linux.rs            Linux reader (process_vm_readv via read-process-memory)
│   ├── macos.rs            macOS reader (mach_vm_read via read-process-memory)
│   ├── discovery.rs        lua_State memory scan + Lua version detection
│   ├── skynet.rs           H -> slots -> snlua.activeL enumeration
│   ├── native_stack.rs     Native C stack: libunwind-ptrace (Linux) / FP walk (macOS)
│   └── spawn.rs            SpawnedChild: exec-stop hold, resume, reap, terminate (RAII)
│
├── symbols/
│   ├── resolver.rs         ELF/Mach-O symbol lookup (addr -> name, name -> addr)
│   └── cache.rs            Bounded FIFO address -> name cache
│
└── output/
    ├── aggregator.rs       Tick accounting + stacked-stack aggregation
    ├── collapsed.rs        Collapsed text + inferno flamegraph SVG + stderr stats
    ├── dump.rs             `dump` mode snapshot formatting
    └── top.rs              `top` mode live view rendering
```

---

## Verified structure layouts

All offsets are hardcoded for **64-bit** targets and cross-checked in tests
(`lua54_matches_reference_structs` uses `offset_of!` against `types.rs`; the 5.5 table is
asserted against documented constants).

### `lua_State` — 200 bytes (5.4) / 208 bytes (5.5)

```
Offset  Field        5.4    5.5     Notes
0       next         8      8       GCObject*
8       tt           1      1       must be LUA_TTHREAD = 8
9       marked       1      1
10      status       1      -       5.5 moves it to 11
11      allowhook    1      -
11      status       -      1
12      nci          2      -
14      (padding)    2      -
16      top          8      8       StkIdRel
24      l_G          8      8       global_State*
32      ci           8      8       CallInfo*  <- current frame
40      stack_last   8      8
48      stack        8      8       StkId (stack base)
56      openupval    8      8
...
96      base_ci      64     64      embedded base CallInfo
...
```

### `CallInfo` — 64 bytes

```
Offset  Field        5.4            5.5
0       func         8              8     stack-relative index (5.4) or pointer (5.5)
8       top          8              8
16      previous     8              8     CallInfo*
24      next         8              8
32      u.l.savedpc  8              8     pc of the *next* instruction
40      u.l.trap     4              4
44      u.l.nextraargs 4            4
48      u2           8              8
56      nresults     2              2
58      callstatus   2 (u16)        -     5.4
60      (padding)    4              -
60      callstatus   -              4 (u32)
```

The `callstatus` width *and* the "C call" bit differ (5.4: `1 << 1`; 5.5: `1 << 15`).
Both live in `LuaLayout` (`callstatus_is_u32`, `cist_c_mask`), but **the walker does not
read `callstatus` at all** — it decodes the function slot's `TValue` tag instead, which is
version-independent and strictly more informative (see "Function decoding").

### `Proto` — 128 bytes

```
Offset  Field            Size
0       next             8
8       tt, marked       2
10      numparams        1
11      is_vararg        1
12      maxstacksize     1
13      (padding)        3
16      sizeupvalues     4 (i32)
20      sizek            4
24      sizecode         4
28      sizelineinfo     4     <- per-instruction line deltas
32      sizep            4
36      sizelocvars      4
40      sizeabslineinfo  4
44      linedefined      4     <- 0 means "main chunk"
48      lastlinedefined  4
52      (padding)        4
56      k                8
64      code             8     Instruction*     <- pc base
72      p                8
80      upvalues         8     Upvaldesc*
88      lineinfo         8     ls_byte*
96      abslineinfo      8     AbsLineInfo*
104     locvars          8
112     source           8     TString*          <- chunk name
120     gclist           8
```

### `LClosure` / `CClosure` — 32 bytes

```
Offset  Field         LClosure        CClosure
0       next          8               8
8       tt, marked    2               2
10      nupvalues     1               1
11      (padding)     5               5
16      gclist        8               8
24      p / f         8 (Proto*)      8 (lua_CFunction)   <- what we read
```

### `TString` — 24 bytes (5.4) / 32 bytes (5.5)

```
Offset  Field        5.4     5.5
0       next         8       8
8       tt           1       1      0x04 short, 0x14 long
9       marked       1       1
10      extra        1       1
11      shrlen       1       1      length of a short string
12      hash         4       4
16      u.lnglen     8       -
24      contents     -       8      pointer to the bytes (5.5 only)
16/24   string data  inline  behind `contents`
```

String length is authoritative: bytes are **not** NUL-terminated. 5.4 puts long-string
bytes inline right after the header; 5.5 puts them behind a `contents` pointer
(`ts_long_is_pointer`).

### `TValue` — 16 bytes

```
Offset  Field   Size
0       value   8        union of gc pointer / 64-bit number / C function
8       tt_     1        tag with the variant packed in the high 4 bits
```

Function tags (`tt & 0x3F`):

| tag | constant | meaning | address used |
|---|---|---|---|
| `0x06` | `LUA_VLCL` | Lua closure | `value` is `LClosure*` -> read `p` |
| `0x16` | `LUA_VLCF` | light C function | `value` **is** the `lua_CFunction` |
| `0x26` | `LUA_VCCL` | C closure | `value` is `CClosure*` -> read `f` |

---

## Lua stack walking

`StackWalker::walk(lua_state)` follows `lua_State.ci` backwards through `CallInfo.previous`
until it reaches the embedded `base_ci`, and returns frames **deepest-first** (current
function at index 0, main chunk last).

### Function decoding

The frame kind comes from the **function slot's `TValue` tag**, not from the `CIST_C` bit of
`callstatus`. `CIST_C` only says "a C function is running"; the slot may hold a light C
function (`value` *is* the function pointer) or a C closure (`value` is a `CClosure*` whose
`ccl_f` holds the real pointer). Trusting `CIST_C` alone reports the closure object's
address as the function address, which is why `[C]:0x…` frames used to be unresolvable.

The function slot address itself is version-dependent: 5.5 stores an absolute pointer
(`stk_is_pointer`), 5.4 stores a stack-relative index that is converted with
`stack_base + index * sizeof(TValue)`.

### Function names

Lua does **not** record a function's own name. `Upvaldesc.name` holds the names of
*captured variables*, so using it labels a function after one of its callees — a
`local function process_data() ... compute_heavy() ... end` would be reported as
`compute_heavy`. Only the main chunk is unambiguous (`linedefined == 0` -> `main`);
everything else is reported as `source:line`, which is what the sampler can actually know.

### Line numbers

`savedpc` points at the *next* instruction; Lua's `currentpc` is `savedpc - 1`. The
`lineinfo` array holds signed byte deltas; accumulating deltas `0..=pc_offset-1` — exactly
`pc_offset` of them — already yields the line of instruction `pc_offset-1`, i.e. the current
instruction. **Do not subtract one from `savedpc` a second time.** The only remaining
off-by-one hazard is the clamp, whose legal maximum is `sizelineinfo` (one entry per
instruction), not `sizelineinfo - 1`.

A `-128` (`ABSLINEINFO_MARKER`) entry redirects to the `abslineinfo` table, whose entries are
`{ int pc; int line; }` (8 bytes, line at +4).

### Defensive reading

Every value comes from a process that is mutating its own memory while we read it, so:

- Reads go through `Pod`-constrained `read_struct` (native-endian byte copy, no pointer
  cast) — sound and alignment-safe for any address the platform reader can serve.
- `validate_addr` rejects null/tiny addresses, kernel space and wrapped pointers.
- Lengths taken from remote memory are bounded: `MAX_LUA_STRING_LEN` (4 KiB),
  `MAX_LINEINFO_BYTES` (1 MiB), `MAX_CSTRING_LEN` (4 KiB).
- All address arithmetic uses `saturating_*`, so half-written (all-ones) pointers fail
  validation instead of panicking in a debug build.
- Loop detection (`visited` set) and `MAX_STACK_DEPTH` are counted over *visited CallInfo
  nodes*, not extracted frames: a CallInfo that fails to decode contributes no frame but
  still costs a remote read.

A single bad frame is logged and skipped; ~99% of samples succeeding is normal, and the
remainder are torn reads.

---

## Native (C) stack

| platform | mechanism |
|---|---|
| Linux x86_64 | `remoteprocess` libunwind-ptrace: seize + interrupt the main thread, walk the unwind cursor, detach |
| Linux aarch64 | none (empty) |
| macOS x86_64 / arm64 | `task_for_pid` + `thread_get_state` + frame-pointer walk |

- **Single-VM only.** A ptrace register snapshot maps to a thread, not to a service, so
  skynet mode deliberately skips the native merge and gives each service a synthetic
  `[service:name]` root frame instead.
- **Merge order** (`Sampler::sample_single`): the aggregator wants the whole stack
  deepest-first. Both the Lua walker and the unwind cursor yield innermost-first, so the
  native chain is appended *after* the Lua frames with no reversing. Prepending a reversed
  native chain puts the process entry point at the leaf.
- **Spawn mode works without root on Linux:** we are the child's parent, so a ptrace seize
  is permitted even under `kernel.yama.ptrace_scope=1`.
- **macOS** requires the target to be traceable (`task_for_pid`), which in practice means
  root — including for a child we spawned ourselves.

---

## Symbol resolution

Two directions are needed:

1. **name -> address** (`resolve_symbol_address`), used to find skynet's `H` global. A
   resolution success means "this is a skynet process"; failure falls back to single-VM
   mode.
2. **address -> name** (`SymbolResolver`), used to label C frames.

Both translate between runtime and file addresses with the same two corrections:

- `file_vaddr = runtime - load_base + link_base`, where `load_base` is the **lowest mapping
  start for that file** (a binary spans several segment mappings) and `link_base` is the
  file's link-time base: the minimum `PT_LOAD` vaddr for ELF, and `__TEXT`'s vmaddr for
  Mach-O. `__PAGEZERO` must be excluded on Mach-O — it has `vmaddr = 0` but maps no file
  content, while a PIE executable's `__TEXT` sits at `0x100000000`.
- Undefined imported symbols (`getenv@GLIBC_2.2.5`) have address 0 and would catch any small
  offset; only defined `Text` symbols are indexed.
- Mach-O stores C symbols with a leading underscore (`H` appears as `_H`), so the
  name -> address direction normalizes the lookup name per object format.

Unresolvable addresses honestly render as `[C]:0x…`. Name lookups are cached in a bounded
FIFO cache (hits do not reorder — resolved addresses stay hot for the whole run).

---

## VM discovery

- **Single VM:** scan writable regions for a structure that looks like a `lua_State`
  (`tt == LUA_TTHREAD`, sane `status`, non-null `ci`/`stack`), then keep the first candidate
  whose stack *actually walks to a non-empty result*. The loose scan alone yields many false
  positives — especially early in a spawned process's life — so the walk is the filter.
  Discovery is lazy: it runs on the first sample tick, not at construction, because in spawn
  mode the VM does not exist yet when the child is frozen at `execve`.
- **skynet:** `H` (`handle_storage`) -> slot array -> `skynet_context.instance` ->
  `snlua.activeL`. Re-enumerated every tick, because services come and go. C services
  (no `activeL`) are skipped.
- **Lua version:** scan the mapped binaries' rodata for the `Lua 5.N` string. This can
  legitimately fail at construction (a dynamically linked interpreter is not mapped yet at
  the exec-stop), in which case the sampler starts on 5.4 and re-detects at most every
  500 ms until a VM confirms the layout. `--lua-version` pins it and disables detection.

---

## Sampling model

- The loop runs at `-r` Hz and honours `-d`; Ctrl+C stops it via an `AtomicBool`.
- attach mode has no `waitpid`, so the loop probes `process_exists(pid)` each tick; spawn
  mode uses `has_exited()`. A reader that fails with `ESRCH` reports
  `ProcessExited` and also ends the run.
- **Ticks vs stacks:** one tick can yield several stacks (skynet). Success is therefore
  accounted per tick: a tick whose stacks are all empty counts as failed. Stack
  percentages in the stats block use the sampled-stack count instead.
- Emitted statistics:

```
Sampling statistics:
  Duration: 5.0s
  Sampling ticks: 500
  Successful ticks: 497
  Failed ticks: 3
  Success rate: 99.4%
  Stacks sampled: 497
  Unique stacks: 4

  Top stacks:
    [33.1%] (95 samples)
      ├─ [C]:pmain
      ├─ tests/lua/multi_func.lua:27:main
      ├─ tests/lua/multi_func.lua:22
      ├─ tests/lua/multi_func.lua:15
      ├─ tests/lua/multi_func.lua:7
      ├─ [C]:math_sin
      ├─ [C]:luaV_execute
      └─ ...
```

Output format follows the extension: `.svg` -> interactive flamegraph (inferno,
bottom-up / icicle), anything else -> collapsed text (`stack;stack;stack count`), which
`flamegraph.pl` accepts.

---

## CLI

```
tracemoon <SUBCOMMAND> [OPTIONS] [-- <COMMAND>...]

SUBCOMMANDS
  record   Record a profile to a flamegraph / collapsed file
  dump     Print a one-shot snapshot of the current stack(s) and exit
  top      Live-updating terminal view of the hottest functions

TARGET (exactly one)
  -p, --pid <PID>          attach to a running process (requires root / CAP_SYS_PTRACE)
  -- <COMMAND>...          spawn the process and profile it from t=0

OPTIONS (record, top)
  -r, --rate <HZ>          sample rate (default 100)
  -d, --duration <SECS>    duration (0 = until Ctrl+C, default 0)
  -o, --output <FILE>      record only (default flamegraph.svg)
      --lua-version <VER>  force the layout: 5.4 or 5.5 (default: auto-detect)
```

`dump` accepts only the target and `--lua-version`. A PID must be a positive integer;
`0` and negative values are rejected, since `kill(0, …)` addresses the process group.

---

## Spawn lifecycle

1. The child sets `PTRACE_TRACEME` in `pre_exec`, so the kernel stops it with `SIGTRAP`
   right after `execve`, before any user code runs.
2. The parent `waitpid`s that exec-stop and builds the sampler while the child is frozen.
3. `ptrace::detach` resumes it — sampling starts at program `t=0`.
4. `has_exited()` is checked each tick (`WNOHANG`); `terminate()` sends `SIGTERM`, waits
   500 ms, then `SIGKILL`, and reaps.
5. `SpawnedChild` terminates on `Drop`, so any early return (failed `Sampler::new`, a
   failing signal-handler install, a spawn that never reaches its exec-stop) still cleans
   up the target instead of leaving an orphan behind. `std::process::Child` is
   `mem::forget`-free: dropping the handle does not reap, which keeps `waitpid` and the exit
   status available to us.

On macOS there is no ptrace-hold: spawn is "start it, then sample immediately".

---

## Dependencies

| crate | role |
|---|---|
| `clap` 3.2 | CLI |
| `remoteprocess` 0.5 (`unwind`) | ptrace seize + libunwind-ptrace unwinding |
| `read-process-memory` 0.1 | cross-platform process memory reads |
| `proc-maps` 0.4 | `/proc/<pid>/maps`-style mappings (both platforms) |
| `object` 0.36 (`read`,`elf`,`macho`) | ELF / Mach-O symbol tables |
| `inferno` 0.12 | flamegraph SVG |
| `nix` 0.27 (`process`,`ptrace`,`signal`) | ptrace, signals, waitpid |
| `libc` 0.2 | Mach / POSIX shims |
| `thiserror`, `log`, `env_logger`, `ctrlc`, `chrono`, `anyhow`, `memmap2` | support |

Rust 1.82+ (`offset_of!`, `Option::is_none_or`).

---

## Platform support

| capability | Linux x86_64 | Linux aarch64 | macOS x86_64 / arm64 |
|---|---|---|---|
| Lua stack walk | yes | yes | yes |
| Native C merge | yes (libunwind-ptrace) | no (empty) | yes (task_for_pid) |
| Spawn without root | yes | yes | no (task_for_pid) |
| Symbol resolution | ELF | ELF | Mach-O |

---

## Debugging

```bash
RUST_LOG=debug tracemoon record -d 2 -o /tmp/out.txt -- lua tests/lua/multi_func.lua
```

Per-frame walk output, TValue tags, symbol lookups and native-unwind failures are all at
`debug`. Two checks catch the classic regressions: no `getenv` / `gcov` frames in the
collapsed output (that means the symbol filter or the load-base math regressed), and a
success rate of ~99% or better (a collapse to ~0% means discovery locked onto a false
positive).
