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
- `src/config.rs` — CLI (clap 3.2 builder API). `record` subcommand.
- `src/sampler.rs` — the coordinator. `Target`, `Mode`, the sample loop, output.
- `src/lua/`
  - `layout.rs` — **version-parameterized offset tables** (`LuaLayout`) + all the typed
    field readers. This is where 5.4 vs 5.5 differences live.
  - `types.rs` — the verified Lua 5.4 `#[repr(C)]` reference structs (used only to
    `offset_of!`-check the 5.4 layout in tests) + `tt` tag helpers + `MAX_STACK_DEPTH`.
  - `stack.rs` — `StackWalker` (CallInfo chain → `StackFrame`s) and `StackFrame` formatting.
- `src/process/`
  - `reader.rs` — `ProcessMemoryReader` trait (object-safe) + `read_struct`/`read_string`
    helpers. Generic reads are free functions because the trait must stay `dyn`-compatible.
  - `linux.rs` / `macos.rs` — platform readers (`process_vm_readv` / `mach_vm_read`).
  - `discovery.rs` — `LuaStateDiscovery` (memory scan for a valid `lua_State`) +
    `detect_lua_version` (scans mapped files' rodata for the `Lua 5.N` string).
  - `skynet.rs` — `SkynetDiscovery`: walk `H` → slots → `snlua.activeL`, one VM per service.
  - `native_stack.rs` — `NativeUnwinder` (Linux x86_64 libunwind-ptrace) + macOS FP walk.
  - `spawn.rs` — `SpawnedChild` (spawn/resume/has_exited/terminate).
- `src/symbols/`
  - `resolver.rs` — ELF/Mach-O symbol resolution (addr → function name) +
    `resolve_symbol_address` (name → runtime addr, used to find skynet's `H`).
  - `cache.rs` — LRU-ish address→name cache.
- `src/output/`
  - `aggregator.rs` — counts identical stacks (collapsed-string keyed).
  - `collapsed.rs` — writes collapsed text + inferno flamegraph SVG + stderr stats.
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
  design: validate pointers (`> 0x1000`, sane ranges), bound lengths, tolerate errors per
  frame/per sample. A single failed sample increments `failed_samples` and the loop
  continues. **Don't add `unwrap()`/`expect()` on memory reads** — return errors.
- A ~99% success rate is expected and healthy; the <1% are torn reads.

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
  `task_for_pid`, unchanged.

### Spawn mode no-missed-startup mechanism (`spawn.rs`)
- Linux: child calls `PTRACE_TRACEME` in `pre_exec` → kernel stops it with `SIGTRAP`
  right after `execve`, before any user code. Parent `waitpid`s that exec-stop, builds the
  sampler while frozen, then `ptrace::detach` resumes it. Sampling begins at program `t=0`.
- The `std::process::Child` handle is `mem::forget`'d so we stay the child's parent and reap
  it ourselves via `waitpid` (exit status stays available after detach).
- Lifecycle: `resume()` once before the loop; `has_exited()` checked each tick (WNOHANG);
  `terminate()` on shutdown = SIGTERM, 500ms grace, then SIGKILL + reap.
- macOS: no ptrace-hold; spawn is best-effort "spawn then sample immediately".

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

1. `cargo test` — unit tests (layout offset checks, config parsing, aggregation, symbol
   formatting, string search). These need no target process.
2. **Functional test in spawn mode** (no root needed) after any change to walking,
   discovery, layout, or symbols:
   - Single VM: `-- lua tests/lua/multi_func.lua` (5.4). Expect ~99% success, real C
     symbols (`lua_gc`, `luaL_tolstring`), correct source:line:func Lua frames.
   - skynet: `-- ./skynet ../benchmark/config` from the skynet dir. Expect `[service:*]`
     roots and each bench case resolving to real C functions (e.g. `luaseri_pack`, `lmd5`).
3. **Sanity checks that catch the classic bugs:** grep the collapsed output for `getenv` or
   `gcov` frames (should be zero), confirm `md5.lua:12→lmd5` / `md5.lua:13→str_gsub`, and
   confirm the success rate is ~99%+ (a crash to ~0% means discovery locked onto a false
   positive).

## Platform / arch support matrix

| | Linux x86_64 | Linux aarch64 | macOS x86_64/arm64 |
|---|---|---|---|
| Lua stack walk | ✅ | ✅ | ✅ |
| Native C merge | ✅ (libunwind-ptrace) | ❌ empty | ✅ (task_for_pid) |
| Spawn no-miss-startup | ✅ (ptrace hold) | ✅ | ⚠️ best-effort |
| Symbol resolution | ELF | ELF | Mach-O |

Primary development/test target is **Linux x86_64**.
