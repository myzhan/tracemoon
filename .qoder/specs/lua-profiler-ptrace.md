# Lua 5.4 Profiler Implementation Spec (tracemoon)

## Overview
A sampling-based Lua 5.4 profiler using process introspection (similar to py-spy). The profiler attaches to running Lua processes, samples both Lua and native C call stacks, resolves function names from debug info and binary symbol tables, and generates interactive flamegraph SVG output.

**Target:** Standard Lua 5.4 interpreter  
**Platforms:** Linux + macOS (x86_64 / ARM64)  
**Output:** Flamegraph SVG (default), collapsed stack format (optional)

---

## Architecture

```
┌──────────────────────────────────────────────────────────┐
│                    Main Entry Point                       │
│        (CLI parsing, permission checks, attach)          │
└────────────────────────┬─────────────────────────────────┘
                         │
                         ▼
┌──────────────────────────────────────────────────────────┐
│                  Sampler Coordinator                      │
│   (Timing loop, sample collection, stack merging)        │
└─────┬──────────┬───────────┬──────────┬──────────────────┘
      │          │           │          │
      ▼          ▼           ▼          ▼
┌──────────┐ ┌──────────┐ ┌──────────┐ ┌────────────────┐
│  State   │ │  Memory  │ │  Stack   │ │     Symbol     │
│Discovery │ │  Reader  │ │  Walker  │ │   Resolution   │
└──────────┘ └──────────┘ └─┬──────┬─┘ └────────────────┘
                            │      │
                     ┌──────┘      └──────┐
                     ▼                    ▼
              ┌────────────┐      ┌────────────────┐
              │ Lua Stack  │      │  Native Stack  │
              │  Walking   │      │   Walking      │
              └────────────┘      └────────────────┘
                    │
         Platform Abstraction Layer
     (Linux: process_vm_readv, macOS: vm_read)
```

---

## Module Structure

```
src/
├── main.rs                 # Entry point, CLI handling, permission checks
├── config.rs               # Configuration and CLI argument parsing (clap)
├── sampler.rs              # Sampling coordinator, stack merging, output
├── error.rs                # Error types (thiserror)
│
├── lua/                    # Lua-specific structures and walking
│   ├── mod.rs
│   ├── types.rs            # Lua 5.4 C struct definitions (#[repr(C)])
│   ├── state.rs            # lua_State reader helper
│   └── stack.rs            # Lua call stack walking + function name extraction
│
├── process/                # Process interaction
│   ├── mod.rs              # Platform-agnostic API
│   ├── reader.rs           # Cross-platform memory reader trait
│   ├── linux.rs            # Linux implementation (read-process-memory)
│   ├── macos.rs            # macOS implementation (read-process-memory)
│   ├── discovery.rs        # lua_State discovery via memory scanning
│   └── native_stack.rs     # Native C stack walking (frame pointer based)
│
├── symbols/                # Symbol resolution
│   ├── mod.rs
│   ├── resolver.rs         # Mach-O/ELF symbol table parsing (object crate)
│   └── cache.rs            # Symbol caching (LRU)
│
└── output/                 # Output generation
    ├── mod.rs
    ├── collapsed.rs        # Collapsed stack format + flamegraph SVG (inferno)
    └── aggregator.rs       # Stack sample aggregation
```

---

## Key Lua 5.4 Structures (Verified Layouts)

All struct layouts verified against Lua 5.4.7 on 64-bit systems:

### LuaState (200 bytes)
```
Offset  Field            Size
0       next             8 (GCObject*)
8       tt               1 (must be LUA_TTHREAD=8)
9       marked           1
10      status           1
11      allowhook        1
12      nci              2
14      [padding]        2
16      top              8 (StkIdRel)
24      l_G              8 (global_State*)
32      ci               8 (CallInfo*)      ← current call info
40      stack_last       8 (StkIdRel)
48      stack            8 (StkId=TValue*)  ← stack base pointer
...
96      base_ci          64 (CallInfo)      ← embedded base CallInfo
...
Total: 200 bytes
```

### CallInfo (64 bytes)
```
Offset  Field            Size
0       func             8 (StkIdRel / absolute pointer)
8       top              8
16      previous         8 (CallInfo*)
24      next             8 (CallInfo*)
32      u.l.savedpc      8
40      u.l.trap         4
44      u.l.nextraargs   4
48      u2               8
56      nresults         2
58      callstatus       2 (CIST_C=0x02 indicates C function)
60      [padding]        4
Total: 64 bytes
```

### Proto (128 bytes)
```
Offset  Field            Size
0       next             8
8       tt, marked       2
10      numparams        1
11      is_vararg        1
12      maxstacksize     1
13-15   [padding]        3
16-48   size fields      36 (9 x i32)
44      linedefined      4
48      lastlinedefined  4
52-55   [padding]        4
56      k                8 (TValue*)
64      code             8 (Instruction*)
72      p                8 (Proto**)
80      upvalues         8 (Upvaldesc*)
88      lineinfo         8 (ls_byte*)
96      abslineinfo      8
104     locvars          8 (LocVar*)
112     source           8 (TString*)      ← source filename
120     gclist           8
Total: 128 bytes
```

### LClosure (32 bytes)
```
Offset  Field            Size
0       next             8
8       tt, marked       2
10      nupvalues        1
11-15   [padding]        5
16      gclist           8
24      p                8 (Proto*)        ← function prototype
Total: 32 bytes
```

### TString (24 bytes)
```
Offset  Field            Size
0       next             8
8       tt               1 (0x04=short, 0x14=long)
9       marked           1
10      extra            1
11      shrlen           1 (length for short strings)
12      hash             4
16      u.lnglen         8 (length for long strings)
Total: 24 bytes (string data follows at offset 24)
```

### TValue (16 bytes)
```
Offset  Field            Size
0       value            8 (union: gc/f/i/n)
8       tt_              1 (type tag with variant bits)
9-15    [padding]        7
Total: 16 bytes
```

**TValue type tags for functions:**
- `0x06` (LUA_VLCL) - Lua closure: value.gc -> LClosure
- `0x16` (LUA_VLCF) - Light C function: value.f IS the function pointer
- `0x26` (LUA_VCCL) - C closure: value.gc -> CClosure

---

## Key Features

### 1. Lua Stack Walking
- Traverses CallInfo linked list from `lua_State.ci` backward via `previous`
- Handles absolute pointer vs relative offset for `CallInfo.func` (auto-detection)
- Loop detection with visited set, max depth 1000
- Three function type handling:
  - **Lua closure (0x06):** Read LClosure -> Proto -> source/line
  - **Light C function (0x16):** Extract function pointer directly from TValue.value
  - **C closure (0x26):** Read CClosure -> function pointer
- Non-function TValues (e.g., numbers during stack transitions) are skipped

### 2. Function Name Extraction
- Reads upvalue names from Proto debug info (Upvaldesc.name -> TString)
- Filters out `_ENV` (global environment upvalue)
- Main chunk (linedefined=0) labeled as "main"
- Anonymous functions shown as `source:line`

### 3. Line Number Calculation
- Reads `lineinfo` array (signed byte relative offsets) from remote process
- Accumulates offsets from `linedefined` to target PC
- Handles `ABSLINEINFO` markers (-128) via `abslineinfo` array lookup
- Falls back to `linedefined` when lineinfo unavailable

### 4. Native (C) Stack Walking
- Captures C-level stack frames via platform APIs:
  - **macOS:** `task_for_pid` + `thread_get_state` (ARM64 / x86_64)
  - **Linux:** `ptrace` getregs
- Frame pointer chain walking (RBP/FP based)
- Merged with Lua stack: native frames at bottom, Lua frames on top
- Generic C frames from Lua stack replaced with detailed native info

### 5. Symbol Resolution
- Parses Mach-O / ELF symbol tables using `object` crate
- ASLR-aware: handles macOS `0x100000000` base address + ASLR slide
- Includes both global (T) and local (t) symbols
- Binary search for address-to-symbol mapping
- Per-binary symbol cache + per-address LRU cache
- Preloads symbols for all mapped binaries on startup
- Fallback: `filename+0xoffset` when symbol not found

### 6. Output Formats
- **Flamegraph SVG (default):** Generated via `inferno` crate, interactive (hover/click-to-zoom)
- **Collapsed stacks:** Text format compatible with `flamegraph.pl` and other tools
- Auto-detected by output file extension (`.svg` vs `.txt`)
- Stats display on stderr with hierarchical tree view

---

## CLI Interface

```
tracemoon record [OPTIONS]

OPTIONS:
  -p, --pid <PID>          PID of a running Lua process (required)
  -r, --rate <HZ>          Sample rate in Hz (default: 100)
  -d, --duration <SECS>    Duration in seconds (0 = until Ctrl+C, default: 0)
  -o, --output <FILE>      Output file (.svg for flamegraph, .txt for collapsed)
                            (default: flamegraph.svg)
```

### Usage Examples
```bash
# Generate flamegraph SVG (default)
sudo tracemoon record -p 12345 -d 10

# Generate collapsed stacks for external tools
sudo tracemoon record -p 12345 -d 10 -o profile.txt

# Custom sample rate
sudo tracemoon record -p 12345 -r 200 -d 5 -o flame.svg
```

---

## Dependencies

```toml
[dependencies]
clap = { version = "3.2", features = ["wrap_help", "cargo", "derive"] }
remoteprocess = { version = "0.5.0", features = ["unwind"] }
thiserror = "1"
log = "0.4"
env_logger = "0.10"
ctrlc = "3"
proc-maps = "0.4"
read-process-memory = "0.1"
memmap2 = "0.9"
object = { version = "0.36", default-features = false, features = ["read", "elf", "macho", "std"] }
inferno = { version = "0.12", default-features = false }
libc = "0.2"
anyhow = "1"
chrono = "0.4"

[target.'cfg(unix)'.dependencies]
nix = { version = "0.27", features = ["process", "ptrace"] }
```

---

## Key Implementation Discoveries

### CallInfo.func addressing
On macOS ARM64, `CallInfo.func` is an **absolute pointer** (not a relative offset from stack base). Detection: if `func > 0x10000 && func > stack_base/2`, treat as absolute pointer.

### Mach-O symbol sizes
Mach-O symbol tables do **not** store symbol sizes (all symbols report `size=0`). Must not filter by `size > 0` when parsing symbols from macOS binaries.

### TString type detection
Use `tt` field (not `extra`) to distinguish short vs long strings:
- `(tt & 0x30) == 0` → short string, length in `shrlen`
- Otherwise → long string, length in `u.lnglen`

### String data offset
String data follows `UTString` (alignment wrapper), which on 64-bit equals `sizeof(TString)` = 24 bytes.

---

## Stats Output Format

```
Sampling statistics:
  Duration: 5.0s
  Samples collected: 288
  Successful samples: 288
  Failed samples: 0
  Unique stacks: 4
  Success rate: 100.0%

  Top stacks:
    [33.1%] (95 samples)
      ├─ [C]:pmain
      ├─ tests/lua/multi_func.lua:27:main
      ├─ tests/lua/multi_func.lua:22:process_data
      ├─ tests/lua/multi_func.lua:15:compute_heavy
      ├─ tests/lua/multi_func.lua:7
      ├─ [C]:math_sin
      ├─ [C]:luaV_execute
      ├─ [C]:luaD_callnoyield
      ├─ ...
      ├─ [C]:main
      └─ [C]:0x196b45d54
```

---

## Notes

- macOS requires `sudo` for `task_for_pid`
- Linux may need `CAP_SYS_PTRACE` or root
- Use `RUST_LOG=debug` for detailed diagnostic logging
- Native stack capture requires frame pointers (`-fno-omit-frame-pointer`)
- Bottom-most `[C]:0x...` frame is typically `dyld` entry point (expected)
