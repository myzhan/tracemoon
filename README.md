# tracemoon

A sampling profiler for Lua 5.4 and 5.5, inspired by [py-spy](https://github.com/benfred/py-spy). Attach to a running Lua process, capture mixed Lua + C call stacks, and generate interactive flamegraph SVGs. It also profiles [skynet](https://github.com/cloudwu/skynet) processes that host many Lua VMs in a single process.

## Features

- **Zero instrumentation** - attaches to running Lua processes without modification
- **Multiple Lua versions** - supports Lua 5.4 and 5.5, auto-detected (or forced with `--lua-version`)
- **skynet multi-VM** - enumerates every service's Lua VM and renders each under a `[service:name]` root frame
- **Mixed stacks** - captures both Lua and native C call stacks, merged together (single-VM mode)
- **Symbol resolution** - resolves C function names from Mach-O/ELF symbol tables
- **Lua debug info** - extracts source files, line numbers, and function names
- **Flamegraph SVG** - generates interactive flamegraphs directly (via [inferno](https://github.com/jonhoo/inferno))
- **Cross-platform** - supports Linux and macOS (x86_64 / ARM64)

## Quick Start

```bash
cargo build --release

# Start a Lua process
lua tests/lua/multi_func.lua &
PID=$!

# Profile for 10 seconds, generate flamegraph
sudo ./target/release/tracemoon record -p $PID -d 10

# Open flamegraph.svg in a browser
open flamegraph.svg
```

## Usage

```
tracemoon record [OPTIONS] [-- <COMMAND>...]

OPTIONS:
  -p, --pid <PID>          Attach to a running Lua process by PID
  -r, --rate <HZ>          Sample rate in Hz (default: 100)
  -d, --duration <SECS>    Duration in seconds (0 = until Ctrl+C, default: 0)
  -o, --output <FILE>      Output file (default: flamegraph.svg)
      --lua-version <VER>  Force the Lua layout: 5.4 or 5.5 (default: auto-detect)
```

tracemoon has two modes; specify exactly one target:

- **Attach** (`-p <PID>`): profile a process that is already running. Anything that
  happened before you attach is not captured.
- **Spawn** (`-- <command>`): tracemoon launches the command itself and profiles it from
  the very start, so startup work isn't missed. On Linux the child is held at the exec
  boundary (via `PTRACE_TRACEME`) until sampling is ready, then resumed. When profiling
  ends (Ctrl+C or the duration limit), the spawned child is terminated (SIGTERM, then
  SIGKILL if needed). If the child exits on its own first, tracemoon stops cleanly. On
  macOS spawn is best-effort and may miss a small startup window.

Output format is determined by file extension:
- `.svg` - interactive flamegraph (default)
- `.txt` - collapsed stacks, compatible with [flamegraph.pl](https://github.com/brendangregg/FlameGraph)

### Examples

```bash
# Default: flamegraph SVG
sudo tracemoon record -p 12345 -d 5

# Collapsed stacks for external tools
sudo tracemoon record -p 12345 -d 5 -o profile.txt

# Higher sample rate, custom output
sudo tracemoon record -p 12345 -r 200 -d 10 -o flame.svg

# Force a Lua version instead of auto-detecting
sudo tracemoon record -p 12345 -d 5 --lua-version 5.5

# Profile a skynet process (multi-VM mode is auto-selected)
sudo tracemoon record -p $(pgrep -f skynet) -d 10 -o skynet.svg

# Spawn mode: launch the target yourself and profile from the start
sudo tracemoon record -d 5 -o out.svg -- lua tests/lua/multi_func.lua

# Spawn a skynet benchmark and capture it from process start
sudo tracemoon record -d 10 -o skynet.svg -- ./skynet ../benchmark/config
```

## Sample Output

```
Sampling statistics:
  Duration: 5.0s
  Samples collected: 288
  Successful samples: 288
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
      ├─ ...
      └─ [C]:main
```

## Requirements

- Rust 1.70+
- Target: Lua 5.4.x / 5.5.x processes (including skynet)
- **Linux:** root or `CAP_SYS_PTRACE` capability
- **macOS:** root (`sudo`) required for `task_for_pid`

## How It Works

1. **Attach** to a running Lua process (via `-p`), or **spawn** the target yourself (via
   `-- <cmd>`) and hold it at the exec boundary so no startup samples are missed
2. **Detect** the Lua version (5.4/5.5) from the target binary's rodata and select the matching memory layout
3. **Discover** the Lua VM(s):
   - Plain Lua: scan process memory for a valid `lua_State`
   - skynet: walk the `H` (`handle_storage`) global → service slots → `snlua.activeL`, one VM per service
4. **Sample** at configurable frequency (default 100 Hz):
   - Walk each VM's Lua `CallInfo` chain for Lua-level stacks
   - Single-VM mode also walks native frame pointers for C-level stacks and merges them
   - skynet mode prepends a `[service:name]` root frame per VM (native merge is skipped, since a
     ptrace register snapshot maps to one worker thread, not a specific VM)
5. **Resolve** symbols:
   - Lua: read `Proto.source` (TString) for filenames, `lineinfo` for line numbers, upvalue names for function names
   - C: parse Mach-O/ELF symbol tables from mapped binaries
6. **Output** aggregated stacks as flamegraph SVG or collapsed text

## Debug Logging

```bash
sudo RUST_LOG=debug ./target/debug/tracemoon record -p 12345 -d 2
```

## Building

```bash
cargo build           # debug
cargo build --release # optimized
cargo test            # run tests
```

## License

MIT
