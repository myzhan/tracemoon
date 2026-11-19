# AGENTS.md

Guidance for AI agents (and humans) working on **tracemoon**. Read this before making
changes. It captures the architecture, the non-obvious invariants, and the workflows that
aren't discoverable from a quick skim of the code.

## What this project is

tracemoon is a **sampling profiler for Lua 5.4 / 5.5**, modeled on
[py-spy](https://github.com/benfred/py-spy). It reads a target process's memory from the
outside (no code injection, no instrumentation), walks the Lua `CallInfo` chain and the
native C stack (libunwind-ptrace on Linux x86_64), aggregates samples, and emits a flamegraph
SVG / collapsed stacks (`record`), a one-shot snapshot (`dump`), or a live top view (`top`). It
has first-class support for **skynet**, which hosts many Lua VMs in one process.

Two targeting modes:
- **Attach** (`-p PID`): profile an already-running process.
- **Spawn** (`-- <cmd>`): tracemoon launches the child itself and profiles from `t=0`, so
  startup work isn't missed (see "Spawn mode" below).

## Build, test, run

```bash
cargo build --release          # optimized binary at target/release/tracemoon
cargo test                     # unit tests (no target process needed)
```

**This dev environment needs a linker workaround** (missing `-dev` symlinks for
liblzma/libunwind). Build with:

```bash
RUSTFLAGS="-L /home/myzhan/github/tracemoon/.linkdeps" cargo build --release
```

The permanent fix is `sudo apt install liblzma-dev libunwind-dev`. `.linkdeps/` is a
gitignored dir of the needed symlinks.

**Testing without root:** attach mode needs root or `CAP_SYS_PTRACE` (and is blocked by
`kernel.yama.ptrace_scope=1`). **Spawn mode does not** — the target is our own child, and
`process_vm_readv` can always read a child we spawned. Prefer spawn mode for local testing:

```bash
# plain single-VM Lua
./target/release/tracemoon record -d 5 -o out.svg -- lua tests/lua/multi_func.lua

# skynet multi-VM (run from inside the skynet dir; benchmark self-exits)
cd ~/github/skynet-test/skynet && \
  tracemoon record -d 20 -o skynet.svg -- ./skynet ../benchmark/config
```

Debug logging: `RUST_LOG=debug ./target/release/tracemoon record ...` (verbose per-frame
walk output).

## Architecture / control flow

```
main.rs                 CLI dispatch: (pid, command) → attach | spawn | error
 └─ config.rs           clap parsing → Config { pid, command, rate, duration, ... }
 └─ sampler.rs          Sampler: owns Target, reader, layout, Mode, resolver, aggregator
     ├─ Target          Attach(Pid) | Spawn(SpawnedChild)
     ├─ Mode            Single { addr: Cell } | Skynet { h_addr }
     ├─ run()           sample loop @ rate Hz; resumes spawned child; honors duration/Ctrl+C
     └─ sample()        → walk Lua stack(s) (+ native merge in single mode) → aggregate
```

Module map:
- `src/config.rs` — CLI (clap 3.2 builder API), `record` / `dump` / `top` subcommands. Also
  owns PID validation (`<= 0` is rejected).
- `src/sampler.rs` — the coordinator. `Target`, `Mode`, the tick loop, output.
- `src/lua/`
  - `layout.rs` — **version-parameterized offset tables** (`LuaLayout`) + all the typed
    field readers. This is where 5.4 vs 5.5 differences live.
  - `types.rs` — the verified Lua 5.4 `#[repr(C)]` reference model (structs, tag constants,
    helpers). Used to `offset_of!`-check the 5.4 layout in tests and to read the C layout;
    the module allows `dead_code` because the profiler reads through `LuaLayout`.
  - `stack.rs` — `StackWalker` (CallInfo chain → `StackFrame`s) and `StackFrame` formatting.
- `src/process/`
  - `reader.rs` — `ProcessMemoryReader` trait (object-safe) + `Pod`-based `read_struct` /
    `read_string` helpers + `validate_addr`. Generic reads are free functions because the
    trait must stay `dyn`-compatible. `#[cfg(test)] mod testing` holds `WindowReader`, the
    in-memory fake every layout/walker test builds its structure image in.
  - `linux.rs` / `macos.rs` — platform readers (`process_vm_readv` / `mach_vm_read`).
  - `discovery.rs` — `LuaStateDiscovery` (memory scan for a valid `lua_State`) +
    `detect_lua_version` (scans mapped files' rodata for the `Lua 5.N` string).
  - `skynet.rs` — `SkynetDiscovery`: walk `H` → slots → `snlua.activeL`, one VM per service.
  - `native_stack.rs` — `NativeUnwinder` (Linux x86_64 libunwind-ptrace) + macOS FP walk
    (with RAII `TaskPort` / `ThreadPorts` guards).
  - `spawn.rs` — `SpawnedChild` (spawn/resume/has_exited/terminate) and its `Drop` cleanup.
- `src/symbols/`
  - `resolver.rs` — ELF/Mach-O symbol resolution (addr → function name) +
    `resolve_symbol_address` (name → runtime addr, used to find skynet's `H`).
  - `cache.rs` — bounded **FIFO** address→name cache (hits do not reorder; it is not LRU).
- `src/output/`
  - `aggregator.rs` — tick accounting + counts identical stacks (collapsed-string keyed).
  - `collapsed.rs` — writes collapsed text + inferno flamegraph SVG + stderr stats.
  - `dump.rs` — `dump` mode snapshot formatting.
  - `top.rs` — `top` mode live view rendering.
- `src/error.rs` — `TracemoonError` enum + `Result` alias.

## Critical invariants & gotchas

These are the things that are easy to break and hard to notice. **Read before editing the
relevant area.**

### Lua memory layout (`layout.rs`)
- Offsets are **hardcoded per version for x86_64 64-bit** targets, verified with
  `offsetof`/`sizeof` against real headers. The 5.4 layout is cross-checked at compile time
  against `types.rs` `#[repr(C)]` structs (`lua54_matches_reference_structs` test). The 5.5
  layout is asserted against documented constants (`lua55_documented_offsets`).
- **Adding a new Lua version = add a `luaNN()` constructor + a `LuaVersion` variant + detect
  string + tests.** Don't scatter version `if`s through the walker; the whole point of
  `LuaLayout` is to keep version differences in one struct.
- Key 5.4↔5.5 differences already handled: `callstatus` width (u16 vs u32) and the
  "C call" bit mask (`0x02` vs `0x8000`); stack values as offsets vs absolute pointers
  (`stk_is_pointer`); long-string bytes inline (5.4) vs behind a `contents` pointer (5.5).
  The `callstatus` fields are documented but **not read** by the walker — see "Function
  decoding" below. They stay in the table so every version delta remains in one struct.

### Function decoding & naming (`stack.rs`)
- The frame kind comes from the **function slot's `TValue` tag**, never from `CIST_C`.
  `CIST_C` only says "a C function is running"; the slot may hold a light C function
  (`value` *is* the `lua_CFunction`) or a C closure (`value` is a `CClosure*` whose `ccl_f`
  holds the real pointer). Trusting `CIST_C` reports the closure object's address as the
  function address, which makes every `[C]:0x…` frame unresolvable.
- **Lua functions have no name.** `Upvaldesc.name` holds the names of *captured variables*,
  so `local function process_data() ... compute_heavy() ... end` used to be reported as
  `compute_heavy`. Only the main chunk (`linedefined == 0`) is named `main`; everything else
  renders as `source:line`. Do not reintroduce upvalue-name lookup.

### Stack walking (`stack.rs`)
- `walk()` returns frames **deepest-first** (current function first, main last). The
  aggregator reverses them for flamegraph base-to-top order. Don't "fix" the order in one
  place without the other.
- Loop detection (`visited` set) and `MAX_STACK_DEPTH` guard against corrupt/torn reads —
  we sample a live process without stopping it, so we *will* occasionally read inconsistent
  memory. Individual bad frames are logged and skipped, not fatal.
- **`calculate_line` already applies Lua's `currentpc = savedpc - 1` semantics** — do NOT
  add an explicit `-1`. The loop reads exactly `pc_offset` (= savedpc index) delta bytes and
  accumulates through the last one, which yields the line of instruction `pc_offset-1` =
  currentpc. Verified empirically: adding a literal `-1` shifts hot call sites onto the
  *following* line (e.g. a call reported at a function's closing `end`). A function sampled
  at two adjacent lines (so appearing as two flamegraph boxes) is expected live-sampling
  skew, not a line-calc bug — don't "fix" it by shifting the offset.

### Sampling reads a running process (no stop-the-world)
- Every read can race with the target mutating its own memory. The code is defensive by
  design: bound lengths, tolerate errors per frame/per sample. **Don't add
  `unwrap()`/`expect()` on memory reads** — return errors.
- `read_struct<T: Pod>` copies native-endian bytes (`from_ne_bytes`); there is no pointer
  cast, so it is sound for any address *and* any alignment the platform reader can serve. A
  blanket `T: Copy` transmute would be unsound (a `bool` holding 2, a `Vec<u8>` buffer whose
  alignment guarantee is 1). Keep the `Pod` list integers-only.
- `validate_addr` rejects `addr < 0x1000`, kernel space and `addr + len` overflow in one
  place. Every remote base + offset computation must go through the `at()` helpers
  (`saturating_add`/`saturating_mul`), so a half-written all-ones pointer fails validation
  instead of panicking in a debug build.
- Lengths read out of remote memory are capped: `MAX_LUA_STRING_LEN` (4 KiB),
  `MAX_LINEINFO_BYTES` (1 MiB), `MAX_CSTRING_LEN` (4 KiB). Never hand a remote length field
  straight to `read_memory`.
- A single failed sample increments the failed-tick counter and the loop continues. A ~99%
  success rate is expected and healthy; the <1% are torn reads.

### Tick accounting (`aggregator.rs`, `collapsed.rs`)
- `Aggregator::add_tick(stacks)` is the only entry point from the loop. Success is counted
  **per tick**, not per stack: one tick yields one stack in single-VM mode but one stack per
  service in skynet mode, and an all-empty tick is a failure. Stack percentages use
  `sampled_stacks()` instead. Mixing the two denominators silently inflates or deflates the
  reported success rate.

### Target-exit detection (`sampler.rs`, `process/mod.rs`)
- attach mode has no `waitpid`: the loop probes `process_exists(pid)` each tick (`/proc/<pid>`
  on Linux, `kill(pid, 0)` on macOS), because the default duration is unlimited and a dead
  target would otherwise spin on failed reads forever.
- A reader that fails with `ESRCH` is mapped to `TracemoonError::ProcessExited`
  (`map_read_error`), so the loop also ends cleanly when the target dies between probes.

### Lua version re-detection (`sampler.rs`)
- `layout` is a `Cell<LuaLayout>` plus a `layout_fixed` flag. Detection at construction can
  legitimately fail (spawn mode freezes the child before the loader maps liblua), so the
  sampler starts on 5.4 and re-detects at most every 500 ms until a VM walks successfully —
  that success is what confirms the layout. `--lua-version` fixes it up front.

### Single-VM discovery is LAZY and validated (`sampler.rs`)
- `Mode::Single { addr: Cell<usize> }` starts at 0. The `lua_State` is discovered on the
  **first sample tick**, not at construction — because in spawn mode the VM doesn't exist
  yet at the exec-stop. Until found, `sample()` returns an empty vec and retries next tick.
- `discover_single_vm()` picks the **first candidate whose stack actually walks to a
  non-empty result**, not just the first `tt==thread` match. The loose scan yields many
  false positives (especially early in a process's life); locking onto one permanently was a
  real bug. Keep the "walk must succeed" filter.

### skynet mode (`skynet.rs`)
- Detected by resolving the `H` symbol (`resolve_symbol_address(pid, "H")`). If it resolves,
  we're in skynet mode; else single mode.
- Re-enumerates every VM **each tick** (services come and go), so it's inherently lazy — no
  Cell needed.
- **Native C merge is intentionally skipped in skynet mode.** A ptrace register snapshot
  maps to one worker thread, not to a specific VM, so merging would attribute C frames to
  the wrong service. Each service gets a synthetic `[service:name]` root frame instead.
- Offsets for `handle_storage`/`skynet_context`/`snlua` are documented at the top of the
  file and verified against skynet's source. C services (no Lua) are skipped via the
  `is_valid_state` check on `activeL`.

### Symbol resolution PIE/bias math (`resolver.rs`)
- Two bugs previously produced wrong names (e.g. "md5.lua calls getenv"). Both fixed; don't
  regress:
  1. **Use the file's load base (lowest mapping start for that path), not the containing
     segment's start.** A binary spans several segment mappings; text is often the *2nd*
     PT_LOAD (vaddr ≠ 0). `file_vaddr = addr - load_base + min_vaddr` where
     `min_vaddr` = lowest loadable segment vaddr.
  2. **Exclude undefined imported symbols** (`getenv@GLIBC`, etc). They have address 0 and
     would catch any small offset. Filter is `kind()==Text && !is_undefined() && addr!=0`.
- Works for both PIE (`min_vaddr==0`) and non-PIE binaries. Unbacked addresses honestly
  fall back to `[C]:0x...` — that's correct, not a bug.
- **Mach-O is different in two ways**, both handled in `link_base()` / `link_name()`:
  1. The link base is `__TEXT`'s vmaddr, *not* the minimum segment vaddr: `__PAGEZERO` has
     `vmaddr = 0` but maps no file content, so a min over segments loses the PIE
     executable's `0x100000000` and every lookup in the main executable misses.
  2. C symbols are stored with a leading underscore (`H` is `_H`), so the name → address
     direction must normalize the lookup name per object format; ELF uses it verbatim.

### Native stack (`native_stack.rs`)
- Linux x86_64: `NativeUnwinder` unwinds the real C chain (`luaV_execute → luaD_call → … →
  main → _libc_start_main`) via `remoteprocess`'s libunwind-ptrace backend. It holds a reusable
  `Unwinder` (built once in `new`, reused every tick), and each `frames()` call does
  `Thread::new(pid)` → `thread.lock()` (ptrace seize+interrupt, detach on drop) → walk
  `unwinder.cursor(&thread)`, innermost-first, capped at 128. Best-effort: any error → empty vec.
  aarch64 returns empty (no unwinder built).
- **Spawn mode works without root:** we are the child's parent, so ptrace seize is permitted
  even under `kernel.yama.ptrace_scope=1`. Attach mode needs the same privileges attach already
  requires.
- **Single-VM only.** `NativeUnwinder` is built only for `Mode::Single` (the interpreter runs on
  the main thread, tid==pid). Skynet keeps native merge disabled — a ptrace snapshot maps to a
  worker thread, not a service.
- **Merge ordering** (`sampler.rs::sample_single`): the aggregator wants the whole stack
  deepest-first (current frame at index 0). Both the Lua walker and the native cursor yield
  innermost-first, so we append the native chain *after* the Lua frames with no reversing. Do
  not reintroduce a `native.rev()` prepend — that inverts the flamegraph (process entry ends up
  at the leaf).
- **`waitpid` caveat:** `Thread::lock` and `SpawnedChild::has_exited` both `waitpid` the same
  child, but run sequentially within a tick (never concurrently), and native failures degrade to
  empty — don't "fix" perceived flakiness by dropping the lock; libunwind-ptrace needs the
  target stopped to read its registers/memory.
- macOS keeps the frame-pointer walk (`NativeStackWalker` + `macos::get_native_frames`) via
  `task_for_pid`. `task_for_pid` hands out a user reference on **every** call and
  `task_threads` transfers one send-right per thread plus the array, so both must be
  deallocated — `TaskPort` / `ThreadPorts` are RAII guards that do it. This runs once per
  tick; leaking here leaks port references at the sampling rate.

### Spawn mode no-missed-startup mechanism (`spawn.rs`)
- Linux: child calls `PTRACE_TRACEME` in `pre_exec` → kernel stops it with `SIGTRAP`
  right after `execve`, before any user code. Parent `waitpid`s that exec-stop, builds the
  sampler while frozen, then `ptrace::detach` resumes it. Sampling begins at program `t=0`.
- The `std::process::Child` handle is **dropped** (never `mem::forget`'d): `Child` has no
  `Drop` impl, so dropping it neither waits nor kills — we stay the parent and reap via
  `waitpid`, which keeps the exit status available after detach.
- Lifecycle: `resume()` once before the loop; `has_exited()` checked each tick (WNOHANG);
  `terminate()` on shutdown = SIGTERM, 500ms grace, then SIGKILL + reap.
- `SpawnedChild` implements `Drop` → `terminate()`. That is the cleanup path for *every*
  early return (failed `Sampler::new`, failing signal-handler install, a spawn that never
  reaches its exec-stop). Removing it orphans the target.
- macOS: no ptrace-hold; spawn is best-effort "spawn then sample immediately", and
  `task_for_pid` still needs root even for our own child.

## Conventions

- **Errors, not panics**, on anything touching target memory or the target process. Reserve
  `expect()` for genuine programmer invariants. CLI arg errors should surface as clean clap
  errors or `TracemoonError::Config`, never a panic (there was a `--lua-version` panic bug).
- Keep version/platform differences behind the existing abstractions (`LuaLayout`,
  `ProcessMemoryReader`, `cfg(target_os/arch)`), not sprinkled inline.
- Comments explain **why** (the non-obvious invariant), not what. The existing module-level
  doc comments are the model — match that density and tone.
- Prefer editing the offset table / adding a layout constructor over special-casing in the
  walker.

## Testing changes

1. `cargo test` — unit tests (layout offset checks, config parsing incl. PID validation,
   aggregation/tick accounting, symbol formatting, string search, and a mock-reader stack
   walk). None of these need a target process: `process::reader::testing::WindowReader` is an
   in-memory byte window, so a test builds a `lua_State`/`CallInfo`/`Proto` image and walks it.
   Add tests there (`stack.rs`) rather than spinning up Lua for behaviour that is pure
   memory layout.
2. `cargo clippy --all-targets -- -D warnings` and `cargo fmt --check` — both are enforced by
   CI (`.github/workflows/ci.yml`). Note the crate is a binary, so an unused `pub` item is a
   `dead_code` warning: delete dead helpers instead of leaving them, and use the targeted
   `allow(dead_code)` only where an item is documentation (the `LuaLayout` table fields,
   `lua/types.rs`).
3. **Functional test in spawn mode** (Linux; no root needed) after any change to walking,
   discovery, layout, or symbols:
   - Single VM: `-- lua tests/lua/multi_func.lua` (5.4). Expect ~99% success, real C
     symbols (`lua_gc`, `luaL_tolstring`), and Lua frames rendered as `source:line` — only
     the main chunk carries a name (`…:27:main`).
   - skynet: `-- ./skynet ../benchmark/config` from the skynet dir. Expect `[service:*]`
     roots and each bench case resolving to real C functions (e.g. `luaseri_pack`, `lmd5`).
4. **Sanity checks that catch the classic bugs:** grep the collapsed output for `getenv` or
   `gcov` frames (should be zero), confirm `md5.lua:12→lmd5` / `md5.lua:13→str_gsub`, and
   confirm the success rate is ~99%+ (a crash to ~0% means discovery locked onto a false
   positive).

> macOS needs root even in spawn mode (`task_for_pid` is the gate, not ptrace), so a local
> macOS run without `sudo` fails with `Inappropriate ioctl for device (os error 25)`. Linux
> (and CI) is the real functional acceptance environment.

## Platform / arch support matrix

| | Linux x86_64 | Linux aarch64 | macOS x86_64/arm64 |
|---|---|---|---|
| Lua stack walk | ✅ | ✅ | ✅ |
| Native C merge | ✅ (libunwind-ptrace) | ❌ empty | ✅ (task_for_pid) |
| Spawn no-miss-startup | ✅ (ptrace hold) | ✅ | ⚠️ best-effort |
| Spawn without root | ✅ | ✅ | ❌ (task_for_pid) |
| Symbol resolution | ELF | ELF | Mach-O |

Primary development/test target is **Linux x86_64**.
